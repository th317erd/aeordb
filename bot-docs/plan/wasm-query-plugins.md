# WASM Query Plugins — Spec

**Date:** 2026-04-09
**Status:** Approved
**Priority:** High — the last major feature for the plugin system

---

## 1. Overview

WASM query plugins are server-side functions deployed as WASM binaries that can read, write, query, and aggregate data in the database — all with the invoking user's permissions. They receive HTTP requests via the `_invoke` endpoint, use host functions to interact with the engine, and return custom-shaped responses to the client.

This completes the plugin system: parsers handle the write path (transforming data on ingest), query plugins handle the read path (custom server-side logic with database access).

---

## 2. Architecture

Three layers:

**Plugin SDK** (`aeordb-plugin-sdk`) — provides `aeordb_query_plugin!` macro + `PluginContext` struct with fluent query builder and all CRUD methods. Runs entirely in WASM. Host function calls are serialized JSON over the WASM boundary.

**Host function bridge** (`wasm_runtime.rs`) — registers 7 host functions in the `"aeordb"` namespace. Each one deserializes args from WASM memory, checks permissions via the `RequestContext`, calls the engine, serializes results back into WASM memory.

**HTTP integration** (`routes.rs`) — the `_invoke` endpoint passes the `RequestContext` (user_id from JWT) into the WASM runtime so host functions can permission-check. The `PluginResponse` status code, content type, and headers are propagated to the HTTP response.

---

## 3. Host Functions (Phase 1: CRUD + Query)

All registered in the `"aeordb"` namespace. All use the same protocol: guest writes JSON args into WASM linear memory, calls host function with `(ptr, len)`, host reads JSON, executes with permission checks, writes JSON response into guest memory via the allocator, returns `(ptr, len)` packed as `i64`.

| Host Function | Args (JSON) | Returns (JSON) | Engine Method |
|---|---|---|---|
| `aeordb_read_file` | `{"path": "/..."}` | `{"data": "<base64>", "content_type": "...", "size": N}` | `DirectoryOps::read_file` |
| `aeordb_write_file` | `{"path": "/...", "data": "<base64>", "content_type": "..."}` | `{"ok": true, "size": N}` | `DirectoryOps::store_file` |
| `aeordb_delete_file` | `{"path": "/..."}` | `{"ok": true}` | `DirectoryOps::delete_file` |
| `aeordb_file_metadata` | `{"path": "/..."}` | `{"path", "size", "content_type", "created_at", "updated_at"}` | `DirectoryOps::get_metadata` |
| `aeordb_list_directory` | `{"path": "/..."}` | `{"entries": [{"name", "type", "size", ...}]}` | `DirectoryOps::list_directory` |
| `aeordb_query` | Full query JSON (same as POST /query) | `{"results": [...], "total_count": N}` | `QueryEngine::execute` |
| `aeordb_aggregate` | Full aggregate JSON | `{"groups": [...]}` | `QueryEngine::execute_aggregate` |

**Data encoding:** File content is base64-encoded in the JSON payloads. This adds ~33% overhead but avoids binary framing complexity. For a 20MB image in a 256MB WASM heap, the base64-encoded form is ~27MB — well within limits. Binary framing is a future optimization.

**Replaces:** The existing `db_read`, `db_write`, `db_delete` stubs are removed and replaced by these host functions.

---

## 4. Permission Model

Plugins run with the invoking user's permissions. The full permission chain applies:

1. `_invoke` endpoint extracts `RequestContext` from JWT claims (user_id + event_bus)
2. `RequestContext` is stored in `HostState` (the WASM store's per-invocation data)
3. Each host function reads the `RequestContext` from `HostState`
4. The permission resolver checks the user's CRUD flags against the requested path
5. Permission denied → host function returns `{"error": "Permission denied", "path": "..."}`
6. Write operations go through the full pipeline (chunking, indexing, directory propagation, events)

A plugin cannot escalate privileges. If user X can't read `/secret/` via the HTTP API, they can't read it via a plugin either.

---

## 5. WASM↔Host Protocol

Same pattern as the existing parser protocol, extended for bidirectional communication:

**Guest → Host (host function call):**
1. Guest serializes args as JSON bytes
2. Guest writes bytes into its own linear memory
3. Guest calls host function with `(ptr: i32, len: i32)`
4. Host reads bytes from guest memory using the `Caller` API
5. Host executes the operation
6. Host allocates space in guest memory via the guest's `alloc` export
7. Host writes response JSON into guest memory
8. Host returns `(response_ptr << 32) | response_len` as `i64`
9. Guest reads response from its own memory

**Guest must export `alloc(size: i32) -> i32`** — a simple bump allocator the host uses to write responses back. The SDK provides this automatically via the macro.

**Host → Guest invocation (the entry point):**
Same as today — host writes `PluginRequest` JSON into guest memory at offset 0, calls `handle(0, len)`, reads response from the returned pointer.

---

## 6. Plugin SDK

### The macro

```rust
aeordb_query_plugin!(handle);

fn handle(ctx: PluginContext, request: PluginRequest) -> Result<PluginResponse, PluginError> {
    // plugin logic here
}
```

Expands to:
- `#[global_allocator]` for WASM
- `#[no_mangle] extern "C" fn alloc(size: i32) -> i32` — allocator for host responses
- `#[no_mangle] extern "C" fn handle(ptr: i32, len: i32) -> i64` — entry point
- Deserialization of `PluginRequest`, construction of `PluginContext` wrapping the FFI layer

### PluginContext

```rust
impl PluginContext {
    // File CRUD
    fn read_file(&self, path: &str) -> Result<FileData, PluginError>;
    fn write_file(&self, path: &str, data: &[u8], content_type: &str) -> Result<(), PluginError>;
    fn delete_file(&self, path: &str) -> Result<(), PluginError>;
    fn file_metadata(&self, path: &str) -> Result<FileMeta, PluginError>;
    fn list_directory(&self, path: &str) -> Result<Vec<DirEntry>, PluginError>;

    // Query — returns a fluent builder
    fn query(&self, path: &str) -> QueryBuilder;
    fn aggregate(&self, path: &str) -> AggregateBuilder;
}
```

Each method serializes its args to JSON, calls the corresponding `aeordb_*` host function via `extern "C"`, deserializes the response, and returns a typed Rust result. The plugin author never sees JSON, pointers, or WASM FFI.

### QueryBuilder (guest-side)

```rust
let results = ctx.query("/users")
    .field("name").contains("Wyatt")
    .field("age").gt_u64(21)
    .limit(10)
    .execute()?;
```

The builder accumulates query nodes in memory. `.execute()` serializes to the same JSON format as `POST /query`, makes one `aeordb_query` host function call, deserializes the response. Zero host calls during construction.

Supports all operations from the engine's QueryBuilder: `eq`, `gt`, `lt`, `between`, `in_values`, `contains`, `similar`, `phonetic`, `fuzzy`, `match_query` — plus typed variants (`eq_u64`, `eq_str`, etc.) and composable `and`/`or`/`not`.

### PluginResponse

```rust
// JSON response
PluginResponse::json(&json!({"users": results}))

// Raw bytes (e.g., transformed image)
PluginResponse::bytes(webp_bytes, "image/webp")

// Stream a file directly (host handles streaming, bytes never enter WASM)
PluginResponse::stream_file("/photos/original.jpg")

// Custom status + headers
PluginResponse::json(&data).with_status(201).with_header("X-Custom", "value")

// Error
PluginResponse::error(404, "User not found")
```

### PluginRequest

```rust
pub struct PluginRequest {
    pub arguments: Vec<u8>,                    // raw request body from client
    pub metadata: HashMap<String, String>,     // path segments, query params, headers
}
```

The `_invoke` endpoint populates `metadata` with: `path` (the URL path), `method` (GET/POST), query parameters, and the `function_name` from the URL segment.

---

## 7. HTTP Integration

### Fix: `_invoke` response propagation

Currently the `_invoke` endpoint always returns 200 with `application/octet-stream`. After this change:

- `PluginResponse.status_code` → HTTP status code
- `PluginResponse.content_type` → `Content-Type` header
- `PluginResponse.headers` → additional HTTP headers
- `PluginResponse.body` → response body

### Fix: `_invoke` request wrapping

Currently the `_invoke` endpoint passes raw body bytes. After this change, it wraps the request in a `PluginRequest` JSON envelope with metadata (path, method, query params, function_name).

### Fix: `function_name` URL segment

Currently ignored. After this change, it's passed in `PluginRequest.metadata["function_name"]` so the plugin can dispatch to different logic based on the URL.

### Memory limit on deploy

The deploy endpoint (`PUT /_deploy`) accepts a `memory_limit` query parameter (e.g., `?memory_limit=256mb`). Stored in the `PluginRecord` and used when invoking the plugin. Defaults to 16MB.

---

## 8. `stream_file` Response Type

When a plugin returns `PluginResponse::stream_file(path)`, the host:
1. Permission-checks the path against the invoking user
2. Opens the file via `DirectoryOps::read_file_streaming`
3. Streams the chunks directly to the HTTP response
4. The file bytes never enter WASM memory

This enables plugins to serve large files (images, videos, archives) without WASM memory constraints. The plugin decides *which* file and *what headers* — the host handles the I/O.

---

## 9. Example: Image CDN Plugin

```rust
use aeordb_plugin_sdk::prelude::*;

aeordb_query_plugin!(handle);

fn handle(ctx: PluginContext, request: PluginRequest) -> Result<PluginResponse, PluginError> {
    let path = request.metadata.get("path")
        .ok_or(PluginError::NotFound)?;
    let format = request.metadata.get("format")
        .map(|s| s.as_str())
        .unwrap_or("jpeg");

    let image_data = ctx.read_file(path)?;
    let transformed = convert_image(&image_data.data, format)?;

    Ok(PluginResponse::bytes(transformed, &format!("image/{}", format)))
}
```

---

## 10. Example: Custom API Plugin

```rust
aeordb_query_plugin!(handle);

fn handle(ctx: PluginContext, request: PluginRequest) -> Result<PluginResponse, PluginError> {
    let function = request.metadata.get("function_name")
        .map(|s| s.as_str())
        .unwrap_or("default");

    match function {
        "search" => {
            let body: serde_json::Value = serde_json::from_slice(&request.arguments)?;
            let term = body["term"].as_str().unwrap_or("");

            let results = ctx.query("/products")
                .field("name").contains(term)
                .field("active").eq_bool(true)
                .limit(20)
                .execute()?;

            PluginResponse::json(&json!({
                "products": results.iter().map(|r| {
                    let meta = ctx.file_metadata(&r.path).ok();
                    json!({"path": r.path, "score": r.score, "meta": meta})
                }).collect::<Vec<_>>()
            }))
        }
        "stats" => {
            let agg = ctx.aggregate("/orders")
                .count()
                .sum("total")
                .group_by("status")
                .execute()?;

            PluginResponse::json(&agg)
        }
        _ => PluginResponse::error(404, &format!("Unknown function: {}", function))
    }
}
```

---

## 11. Implementation Phases

### Phase 1 — Host functions + PluginContext (CRUD + Query)
- Replace `db_read`/`db_write`/`db_delete` stubs with 7 real host functions
- `RequestContext` in `HostState` for permission checking
- `alloc` export protocol for host → guest responses
- SDK: `PluginContext` with CRUD methods + `QueryBuilder`
- SDK: `aeordb_query_plugin!` macro
- Fix `_invoke` response propagation (status, content_type, headers)
- Fix `_invoke` request wrapping (PluginRequest envelope with metadata)
- Tests: CRUD host functions, query host function, permission enforcement

### Phase 2 — QueryBuilder fluent API
- Full guest-side QueryBuilder mirroring the engine's builder
- All operations: eq, gt, lt, between, in, contains, similar, phonetic, fuzzy, match
- Typed variants: eq_u64, eq_str, eq_f64, gt_u64, etc.
- Composable: and/or/not
- AggregateBuilder: count, sum, avg, min, max, group_by
- Tests: builder serialization, round-trip query execution

### Phase 3 — stream_file + memory limits
- `PluginResponse::stream_file(path)` type
- Host detects stream_file response, streams via DirectoryOps
- Per-plugin memory_limit in PluginRecord + deploy endpoint
- Tests: large file streaming, memory limit enforcement

### Phase 4 — E2E integration
- Build a real query plugin (similar to the examples above)
- Deploy via HTTP, invoke via HTTP, verify custom response shape
- Permission enforcement E2E
- Multi-function dispatch via function_name

---

## 12. Non-goals (deferred)

- Server-side compilation (developers compile WASM locally)
- Chunked/streaming file reads inside WASM (use memory limits instead)
- Persistent WASM instances across invocations (fresh instance per call)
- Plugin-to-plugin communication
- Versioning/snapshot host functions (Phase 2 of the broader plugin roadmap)
- Upload protocol host functions (Phase 3 of the broader plugin roadmap)
- Binary framing for file data (base64 JSON for v1)
