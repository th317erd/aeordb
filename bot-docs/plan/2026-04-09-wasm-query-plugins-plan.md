# WASM Query Plugins Implementation Plan (Phases 1+2)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable WASM plugins to read, write, query, and aggregate data with the invoking user's permissions, returning custom-shaped responses.

**Architecture:** Replace the 3 stub host functions with 7 real host functions backed by the engine. Add `PluginContext` and `QueryBuilder` to the SDK. Fix `_invoke` to propagate PluginResponse status/headers. The host passes `RequestContext` into `HostState` so host functions can permission-check.

**Tech Stack:** Rust, wasmi, serde_json, base64, aeordb-plugin-sdk

**Spec:** `bot-docs/plan/wasm-query-plugins.md`

---

## File Structure

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `aeordb-lib/src/plugins/wasm_runtime.rs` | Replace stubs with 7 real host functions, add `EngineHandle` + `RequestContext` to `HostState`, add `alloc` export support |
| Modify | `aeordb-lib/src/plugins/plugin_manager.rs` | Pass `Arc<StorageEngine>` + `RequestContext` into runtime, add `memory_limit` to `PluginRecord` |
| Modify | `aeordb-lib/src/server/routes.rs` | Fix `invoke_plugin`: wrap request in `PluginRequest` envelope, propagate `PluginResponse` status/content_type/headers, pass `RequestContext` |
| Create | `aeordb-plugin-sdk/src/context.rs` | `PluginContext` with CRUD methods + query/aggregate, FFI wrappers for host function calls |
| Create | `aeordb-plugin-sdk/src/query_builder.rs` | Guest-side `QueryBuilder` + `FieldQueryBuilder` + `AggregateBuilder`, serializes to JSON, executes via host function |
| Modify | `aeordb-plugin-sdk/src/lib.rs` | Add `aeordb_query_plugin!` macro, `alloc` export, module declarations, prelude updates |
| Modify | `aeordb-plugin-sdk/Cargo.toml` | No new deps needed (serde_json + base64 already present) |
| Create | `aeordb-lib/spec/engine/wasm_query_plugin_spec.rs` | Host function tests (CRUD + query + permissions) |
| Create | `aeordb-lib/spec/http/plugin_invoke_spec.rs` | HTTP-level tests (_invoke endpoint, response propagation) |
| Modify | `aeordb-lib/Cargo.toml` | Add test entries |

---

### Task 1: HostState with Engine Access + RequestContext

**Files:**
- Modify: `aeordb-lib/src/plugins/wasm_runtime.rs`
- Modify: `aeordb-lib/src/plugins/plugin_manager.rs`

The `HostState` currently holds only `memory: Option<Memory>`. It needs access to the engine and the invoking user's context.

- [ ] **Step 1: Expand HostState**

In `wasm_runtime.rs`, change `HostState` to:

```rust
use std::sync::Arc;
use crate::engine::StorageEngine;
use crate::engine::RequestContext;

struct HostState {
    memory: Option<Memory>,
    engine: Option<Arc<StorageEngine>>,
    request_context: Option<RequestContext>,
}
```

- [ ] **Step 2: Add engine + context parameters to call_handle**

Add a new method `call_handle_with_context` that accepts `Arc<StorageEngine>` and `RequestContext`:

```rust
pub fn call_handle_with_context(
    &self,
    request_bytes: &[u8],
    engine: Arc<StorageEngine>,
    ctx: RequestContext,
) -> Result<Vec<u8>, WasmRuntimeError> {
    // Same as call_handle but stores engine + ctx in HostState
}
```

Keep the existing `call_handle` for backward compatibility (parsers don't need engine access).

- [ ] **Step 3: Update PluginManager to pass engine + context**

In `plugin_manager.rs`, add a new invoke method:

```rust
pub fn invoke_wasm_plugin_with_context(
    &self,
    path: &str,
    request_bytes: &[u8],
    engine: Arc<StorageEngine>,
    ctx: RequestContext,
) -> Result<Vec<u8>, PluginManagerError> {
    // Same as invoke_wasm_plugin but uses call_handle_with_context
}
```

- [ ] **Step 4: Run existing tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test -- --test-threads=4`
Expected: All existing tests pass (no behavior change yet)

- [ ] **Step 5: Commit**

```bash
git add aeordb-lib/src/plugins/wasm_runtime.rs aeordb-lib/src/plugins/plugin_manager.rs
git commit -m "WASM Query Phase 1.1: HostState with engine access + RequestContext"
```

---

### Task 2: Implement 7 Host Functions

**Files:**
- Modify: `aeordb-lib/src/plugins/wasm_runtime.rs`
- Create: `aeordb-lib/spec/engine/wasm_query_plugin_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

Replace the 3 stub host functions (`db_read`, `db_write`, `db_delete`) with 7 real ones. Each host function:
1. Reads JSON args from guest memory at `(ptr, len)`
2. Deserializes the args
3. Checks permissions via the `RequestContext` in `HostState`
4. Calls the engine
5. Serializes the result as JSON
6. Calls the guest's `alloc` export to get a buffer
7. Writes the result into guest memory
8. Returns `(result_ptr << 32) | result_len` as `i64`

The 7 host functions and their JSON protocols:

**`aeordb_read_file`** — args: `{"path":"/..."}`, returns: `{"data":"<base64>","content_type":"...","size":N}` or `{"error":"..."}`

**`aeordb_write_file`** — args: `{"path":"/...","data":"<base64>","content_type":"..."}`, returns: `{"ok":true,"size":N}` or `{"error":"..."}`

**`aeordb_delete_file`** — args: `{"path":"/..."}`, returns: `{"ok":true}` or `{"error":"..."}`

**`aeordb_file_metadata`** — args: `{"path":"/..."}`, returns: `{"path":"...","size":N,"content_type":"...","created_at":N,"updated_at":N}` or `{"error":"..."}`

**`aeordb_list_directory`** — args: `{"path":"/..."}`, returns: `{"entries":[{"name":"...","type":"file"|"directory","size":N},...]}` or `{"error":"..."}`

**`aeordb_query`** — args: same JSON as POST /query body (`{"path":"/...","where":{...},"limit":N,...}`), returns: `{"results":[{"path":"...","score":N,...}...],"total_count":N}` or `{"error":"..."}`

**`aeordb_aggregate`** — args: same JSON as aggregate query, returns: `{"groups":[...],"total":N}` or `{"error":"..."}`

Implementation pattern for each host function (they all follow the same structure):

```rust
linker.func_wrap(
    "aeordb",
    "aeordb_read_file",
    |mut caller: Caller<'_, HostState>, arg_ptr: i32, arg_len: i32| -> i64 {
        // 1. Read args from guest memory
        let memory = caller.data().memory.unwrap();
        let mut arg_buf = vec![0u8; arg_len as usize];
        memory.read(&caller, arg_ptr as usize, &mut arg_buf).unwrap_or_default();

        // 2. Deserialize args
        let args: serde_json::Value = serde_json::from_slice(&arg_buf).unwrap_or_default();
        let path = args["path"].as_str().unwrap_or("");

        // 3. Execute with engine (from HostState)
        let engine = caller.data().engine.as_ref().unwrap().clone();
        let ctx = caller.data().request_context.as_ref().unwrap().clone();

        // 4. Permission check + engine call
        let result_json = match execute_read_file(&engine, &ctx, path) {
            Ok(data) => data,
            Err(e) => serde_json::json!({"error": e.to_string()}),
        };

        // 5. Serialize result
        let result_bytes = serde_json::to_vec(&result_json).unwrap_or_default();

        // 6. Write result to guest memory via alloc
        let alloc_fn = caller.get_export("alloc")
            .and_then(|e| e.into_func())
            .and_then(|f| f.typed::<i32, i32>(&caller).ok());

        if let Some(alloc) = alloc_fn {
            let ptr = alloc.call(&mut caller, result_bytes.len() as i32).unwrap_or(0);
            let memory = caller.data().memory.unwrap();
            memory.write(&mut caller, ptr as usize, &result_bytes).unwrap_or_default();
            ((ptr as i64) << 32) | (result_bytes.len() as i64)
        } else {
            0i64
        }
    },
)?;
```

For the engine calls, create helper functions:

```rust
fn execute_read_file(engine: &StorageEngine, ctx: &RequestContext, path: &str) -> EngineResult<serde_json::Value> {
    let ops = DirectoryOps::new(engine);
    let data = ops.read_file(path)?;
    let metadata = ops.get_metadata(path)?
        .ok_or_else(|| EngineError::NotFound(path.to_string()))?;
    Ok(serde_json::json!({
        "data": base64::engine::general_purpose::STANDARD.encode(&data),
        "content_type": metadata.content_type.unwrap_or_default(),
        "size": data.len(),
    }))
}
```

Similar helpers for write, delete, metadata, list, query, aggregate.

**Permission checking:** Use the existing `PermissionResolver` from the engine. For reads: check `CrudlifyOp::Read`. For writes: check `CrudlifyOp::Create` or `CrudlifyOp::Update`. For deletes: check `CrudlifyOp::Delete`. For list: check `CrudlifyOp::List`. The resolver is already used by the HTTP middleware — reuse the same logic.

Note: In `--auth=false` mode, permissions are not enforced. The `RequestContext::system()` context bypasses all permission checks. This is consistent with the HTTP API behavior.

- [ ] **Step 1: Remove old stubs, implement 7 host functions**

Replace everything in `register_host_functions` (except `log_message` which stays). Add the 7 new host functions plus helper execution functions.

- [ ] **Step 2: Add `alloc` handling**

The host needs to call the guest's `alloc` export to write response data back. The guest exports `alloc(size: i32) -> i32` which returns a pointer to `size` bytes of memory. The host uses this to write JSON responses back into guest memory.

In `call_handle_with_context`, after instantiation, resolve `alloc`:
```rust
let alloc_fn = instance.get_func(&store, "alloc");
// Store in HostState or resolve per-call in host functions
```

- [ ] **Step 3: Add test entry to Cargo.toml**

```toml
[[test]]
name = "wasm_query_plugin_spec"
path = "spec/engine/wasm_query_plugin_spec.rs"

[[test]]
name = "plugin_invoke_spec"
path = "spec/http/plugin_invoke_spec.rs"
```

- [ ] **Step 4: Write host function tests**

Create `aeordb-lib/spec/engine/wasm_query_plugin_spec.rs`. These tests use the engine directly (not HTTP) to verify host functions work.

Since we can't easily test WASM host functions without a WASM module, the tests should:
1. Build a minimal WASM query plugin that exercises each host function
2. Deploy it, invoke it, verify the results

Alternative (simpler for v1): test the `execute_*` helper functions directly — they take `(&StorageEngine, &RequestContext, &str)` and return `EngineResult<serde_json::Value>`. This bypasses the WASM boundary but verifies the engine integration.

Test cases:
- `test_read_file_returns_base64_content` — store file, call execute_read_file, verify base64 data decodes to original
- `test_write_file_creates_file` — call execute_write_file with base64 data, read it back
- `test_delete_file_removes_file` — store, delete, verify gone
- `test_file_metadata_returns_correct_fields` — store, get metadata, verify size/content_type/timestamps
- `test_list_directory_returns_entries` — store 3 files in /docs/, list /docs/, verify 3 entries
- `test_query_returns_matching_results` — store indexed JSON files, query, verify matches
- `test_aggregate_returns_groups` — store indexed files, aggregate, verify counts

- [ ] **Step 5: Run tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test --test wasm_query_plugin_spec -- --test-threads=1`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add aeordb-lib/src/plugins/wasm_runtime.rs aeordb-lib/spec/engine/wasm_query_plugin_spec.rs aeordb-lib/Cargo.toml
git commit -m "WASM Query Phase 1.2: 7 real host functions (CRUD + query + aggregate)"
```

---

### Task 3: SDK — PluginContext + aeordb_query_plugin! macro

**Files:**
- Create: `aeordb-plugin-sdk/src/context.rs`
- Modify: `aeordb-plugin-sdk/src/lib.rs`

This task builds the guest-side SDK that plugin authors use. The `PluginContext` wraps FFI calls to host functions. The `aeordb_query_plugin!` macro generates the WASM entry point.

- [ ] **Step 1: Create context.rs — PluginContext**

Create `aeordb-plugin-sdk/src/context.rs`:

```rust
use serde::{Deserialize, Serialize};
use crate::PluginError;

// FFI declarations for host functions
extern "C" {
    fn aeordb_read_file(ptr: i32, len: i32) -> i64;
    fn aeordb_write_file(ptr: i32, len: i32) -> i64;
    fn aeordb_delete_file(ptr: i32, len: i32) -> i64;
    fn aeordb_file_metadata(ptr: i32, len: i32) -> i64;
    fn aeordb_list_directory(ptr: i32, len: i32) -> i64;
    fn aeordb_query(ptr: i32, len: i32) -> i64;
    fn aeordb_aggregate(ptr: i32, len: i32) -> i64;
}

/// Database context available to query plugins.
/// Provides CRUD, query, and aggregate operations — all permission-checked
/// against the invoking user.
pub struct PluginContext;

impl PluginContext {
    pub(crate) fn new() -> Self {
        PluginContext
    }

    pub fn read_file(&self, path: &str) -> Result<FileData, PluginError> {
        let args = serde_json::json!({"path": path});
        let result = call_host_function(aeordb_read_file, &args)?;
        check_error(&result)?;
        Ok(FileData {
            data: base64_decode(result["data"].as_str().unwrap_or(""))?,
            content_type: result["content_type"].as_str().unwrap_or("").to_string(),
            size: result["size"].as_u64().unwrap_or(0),
        })
    }

    pub fn write_file(&self, path: &str, data: &[u8], content_type: &str) -> Result<(), PluginError> {
        let args = serde_json::json!({
            "path": path,
            "data": base64_encode(data),
            "content_type": content_type,
        });
        let result = call_host_function(aeordb_write_file, &args)?;
        check_error(&result)?;
        Ok(())
    }

    pub fn delete_file(&self, path: &str) -> Result<(), PluginError> {
        let args = serde_json::json!({"path": path});
        let result = call_host_function(aeordb_delete_file, &args)?;
        check_error(&result)?;
        Ok(())
    }

    pub fn file_metadata(&self, path: &str) -> Result<FileMeta, PluginError> {
        let args = serde_json::json!({"path": path});
        let result = call_host_function(aeordb_file_metadata, &args)?;
        check_error(&result)?;
        serde_json::from_value(result)
            .map_err(|e| PluginError::SerializationFailed(e.to_string()))
    }

    pub fn list_directory(&self, path: &str) -> Result<Vec<DirEntry>, PluginError> {
        let args = serde_json::json!({"path": path});
        let result = call_host_function(aeordb_list_directory, &args)?;
        check_error(&result)?;
        let entries = result["entries"].as_array()
            .ok_or_else(|| PluginError::SerializationFailed("missing entries".to_string()))?;
        entries.iter().map(|e| {
            serde_json::from_value(e.clone())
                .map_err(|e| PluginError::SerializationFailed(e.to_string()))
        }).collect()
    }

    pub fn query(&self, path: &str) -> crate::query_builder::QueryBuilder {
        crate::query_builder::QueryBuilder::new(path)
    }

    pub fn aggregate(&self, path: &str) -> crate::query_builder::AggregateBuilder {
        crate::query_builder::AggregateBuilder::new(path)
    }
}

// Helper: call a host function with JSON args, return JSON result
fn call_host_function(
    host_fn: unsafe extern "C" fn(i32, i32) -> i64,
    args: &serde_json::Value,
) -> Result<serde_json::Value, PluginError> {
    let arg_bytes = serde_json::to_vec(args)
        .map_err(|e| PluginError::SerializationFailed(e.to_string()))?;

    let packed = unsafe { host_fn(arg_bytes.as_ptr() as i32, arg_bytes.len() as i32) };

    let result_ptr = (packed >> 32) as u32 as usize;
    let result_len = (packed & 0xFFFF_FFFF) as u32 as usize;

    if result_len == 0 {
        return Err(PluginError::ExecutionFailed("empty response from host".to_string()));
    }

    let result_bytes = unsafe {
        std::slice::from_raw_parts(result_ptr as *const u8, result_len)
    };

    serde_json::from_slice(result_bytes)
        .map_err(|e| PluginError::SerializationFailed(e.to_string()))
}

fn check_error(result: &serde_json::Value) -> Result<(), PluginError> {
    if let Some(error) = result.get("error").and_then(|e| e.as_str()) {
        if error.contains("Permission denied") {
            return Err(PluginError::ExecutionFailed(error.to_string()));
        }
        return Err(PluginError::ExecutionFailed(error.to_string()));
    }
    Ok(())
}

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn base64_decode(encoded: &str) -> Result<Vec<u8>, PluginError> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(encoded)
        .map_err(|e| PluginError::SerializationFailed(format!("base64 decode: {}", e)))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileData {
    pub data: Vec<u8>,
    pub content_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub size: u64,
}
```

- [ ] **Step 2: Add `aeordb_query_plugin!` macro to lib.rs**

In `aeordb-plugin-sdk/src/lib.rs`, add the macro and module declarations:

```rust
pub mod context;
pub mod query_builder;

/// Generate the WASM exports for a query plugin.
#[macro_export]
macro_rules! aeordb_query_plugin {
    ($handler_fn:ident) => {
        #[cfg(target_arch = "wasm32")]
        #[global_allocator]
        static ALLOC: std::alloc::System = std::alloc::System;

        /// WASM export: alloc(size) -> ptr
        /// Used by the host to allocate guest memory for writing response data back.
        #[no_mangle]
        pub extern "C" fn alloc(size: i32) -> i32 {
            let mut buf = Vec::with_capacity(size as usize);
            let ptr = buf.as_mut_ptr();
            std::mem::forget(buf);
            ptr as i32
        }

        /// WASM export: handle(request_ptr, request_len) -> packed i64
        #[no_mangle]
        pub extern "C" fn handle(ptr: i32, len: i32) -> i64 {
            let request_bytes = unsafe {
                std::slice::from_raw_parts(ptr as *const u8, len as usize)
            };

            let request: $crate::PluginRequest = match serde_json::from_slice(request_bytes) {
                Ok(req) => req,
                Err(e) => {
                    let resp = $crate::PluginResponse::error(400, &format!("Invalid request: {}", e));
                    return encode_plugin_response(&resp);
                }
            };

            let ctx = $crate::context::PluginContext::new();

            let response = match $handler_fn(ctx, request) {
                Ok(resp) => resp,
                Err(e) => $crate::PluginResponse::error(500, &e.to_string()),
            };

            encode_plugin_response(&response)
        }

        fn encode_plugin_response(response: &$crate::PluginResponse) -> i64 {
            let bytes = serde_json::to_vec(response).unwrap_or_default();
            let len = bytes.len();
            let ptr = bytes.as_ptr() as i64;
            std::mem::forget(bytes);
            (ptr << 32) | (len as i64)
        }
    };
}
```

Update the prelude:
```rust
pub mod prelude {
    pub use super::{HostFunctions, PluginError, PluginRequest, PluginResponse};
    pub use super::parser::{ParserInput, FileMeta};
    pub use super::context::{PluginContext, FileData, DirEntry};
}
```

- [ ] **Step 3: Verify SDK compiles**

Run: `cd /home/wyatt/Projects/aeordb && cargo check -p aeordb-plugin-sdk`
Expected: Compiles (the extern "C" functions are only linked when targeting wasm32)

- [ ] **Step 4: Commit**

```bash
git add aeordb-plugin-sdk/src/context.rs aeordb-plugin-sdk/src/lib.rs
git commit -m "WASM Query Phase 1.3: SDK PluginContext + aeordb_query_plugin! macro"
```

---

### Task 4: SDK — QueryBuilder + AggregateBuilder

**Files:**
- Create: `aeordb-plugin-sdk/src/query_builder.rs`

The guest-side QueryBuilder mirrors the engine's QueryBuilder API but serializes to JSON instead of executing directly. `.execute()` calls the `aeordb_query` host function.

- [ ] **Step 1: Create query_builder.rs**

Create `aeordb-plugin-sdk/src/query_builder.rs`:

The builder needs to produce JSON matching the POST /query format:
```json
{
    "path": "/users",
    "where": {
        "AND": [
            {"field": "name", "op": "contains", "value": "Wyatt"},
            {"field": "age", "op": "gt", "value": 21}
        ]
    },
    "limit": 10
}
```

Read `aeordb-lib/src/server/engine_routes.rs` to understand the exact JSON query format the server accepts. The QueryBuilder must produce this exact format.

Key types:
```rust
pub struct QueryBuilder {
    path: String,
    nodes: Vec<QueryNode>,
    limit_value: Option<usize>,
    sort_fields: Vec<SortField>,
}

pub struct FieldQueryBuilder {
    parent: QueryBuilder,
    field_name: String,
}

enum QueryNode {
    Field { field: String, op: String, value: serde_json::Value },
    And(Vec<QueryNode>),
    Or(Vec<QueryNode>),
    Not(Box<QueryNode>),
}

pub struct AggregateBuilder {
    path: String,
    operations: Vec<String>,
    group_by: Option<String>,
    where_clause: Option<QueryNode>,
}
```

QueryBuilder methods (returning self for chaining):
- `field(name) -> FieldQueryBuilder`
- `and(|q| q.field(...)) -> QueryBuilder`
- `or(|q| q.field(...)) -> QueryBuilder`
- `not(|q| q.field(...)) -> QueryBuilder`
- `limit(n) -> QueryBuilder`
- `sort(field, direction) -> QueryBuilder`
- `execute() -> Result<Vec<QueryResult>, PluginError>` — serializes to JSON, calls aeordb_query host function

FieldQueryBuilder methods (returning parent QueryBuilder):
- `eq(value)`, `eq_u64(n)`, `eq_str(s)`, `eq_f64(f)`, `eq_bool(b)`
- `gt(value)`, `gt_u64(n)`, `gt_str(s)`, `gt_f64(f)`
- `lt(value)`, `lt_u64(n)`, `lt_str(s)`, `lt_f64(f)`
- `between(min, max)`, `between_u64(min, max)`, `between_str(min, max)`
- `in_values(values)`, `in_u64(values)`, `in_str(values)`
- `contains(text)`, `similar(text, threshold)`, `phonetic(text)`, `fuzzy(text)`, `match_query(text)`

AggregateBuilder methods:
- `count() -> Self`, `sum(field) -> Self`, `avg(field) -> Self`, `min(field) -> Self`, `max(field) -> Self`
- `group_by(field) -> Self`
- `where_clause(|q| q.field(...)) -> Self`
- `execute() -> Result<AggregateResult, PluginError>`

QueryResult type (deserialized from host response):
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub path: String,
    pub score: f64,
    pub matched_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregateResult {
    pub groups: Vec<serde_json::Value>,
    pub total_count: Option<u64>,
}
```

- [ ] **Step 2: Verify SDK compiles**

Run: `cd /home/wyatt/Projects/aeordb && cargo check -p aeordb-plugin-sdk`
Expected: Compiles

- [ ] **Step 3: Commit**

```bash
git add aeordb-plugin-sdk/src/query_builder.rs
git commit -m "WASM Query Phase 2: SDK QueryBuilder + AggregateBuilder with fluent API"
```

---

### Task 5: Fix _invoke HTTP Endpoint

**Files:**
- Modify: `aeordb-lib/src/server/routes.rs`

Currently `invoke_plugin` passes raw bytes and always returns 200 with `application/octet-stream`. Fix it to:
1. Wrap the request in a `PluginRequest` JSON envelope with metadata
2. Pass `Arc<StorageEngine>` + `RequestContext` to the plugin manager
3. Deserialize the `PluginResponse` from the WASM output
4. Map `PluginResponse.status_code` to HTTP status
5. Map `PluginResponse.content_type` to Content-Type header
6. Map `PluginResponse.headers` to additional HTTP headers
7. Return `PluginResponse.body` as the response body

The `PluginRequest` metadata should include:
- `path` — the URL path
- `function_name` — from the URL segment (currently ignored)
- Query parameters from the URL

- [ ] **Step 1: Update invoke_plugin handler**

Read the current handler at `routes.rs:89-126`. Replace with the new version that wraps the request, passes context, and propagates the response.

The handler needs access to `Extension(claims): Extension<TokenClaims>` — add it to the function signature (same as other protected routes).

- [ ] **Step 2: Write HTTP-level tests**

Create `aeordb-lib/spec/http/plugin_invoke_spec.rs`:

Tests (these test the HTTP integration, not the WASM internals):
- `test_invoke_returns_plugin_status_code` — deploy a simple plugin that returns status 201, verify HTTP 201
- `test_invoke_returns_plugin_content_type` — plugin returns content_type "text/csv", verify header
- `test_invoke_returns_plugin_body` — plugin returns custom JSON body, verify it
- `test_invoke_not_found_for_missing_plugin` — invoke nonexistent plugin → 404
- `test_invoke_passes_function_name_in_metadata` — plugin reads function_name from request metadata
- `test_invoke_requires_auth` — no token → 401

Note: These tests need a deployed WASM query plugin. The simplest approach is to build a minimal test plugin that echoes back the request metadata or returns a hardcoded response. Alternatively, test the helper functions unit-test style.

- [ ] **Step 3: Run tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test --test plugin_invoke_spec -- --test-threads=1`
Expected: All tests pass

- [ ] **Step 4: Run full suite**

Run: `cd /home/wyatt/Projects/aeordb && cargo test -- --test-threads=4`
Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add aeordb-lib/src/server/routes.rs aeordb-lib/spec/http/plugin_invoke_spec.rs
git commit -m "WASM Query Phase 1.5: fix _invoke to propagate PluginResponse + pass RequestContext"
```

---

### Task 6: Build + Test a Real Query Plugin

**Files:**
- Create: `aeordb-plugins/echo-plugin/Cargo.toml`
- Create: `aeordb-plugins/echo-plugin/src/lib.rs`
- Create: `aeordb-lib/spec/engine/wasm_query_e2e_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

Build a minimal but real query plugin that exercises the full stack: deploy WASM, invoke via engine, verify host functions work end-to-end.

- [ ] **Step 1: Create the echo-plugin crate**

Create `aeordb-plugins/echo-plugin/Cargo.toml`:
```toml
[package]
name = "aeordb-echo-plugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
aeordb-plugin-sdk = { path = "../../aeordb-plugin-sdk" }
serde_json = { workspace = true }
```

Create `aeordb-plugins/echo-plugin/src/lib.rs`:
```rust
use aeordb_plugin_sdk::prelude::*;
use aeordb_plugin_sdk::aeordb_query_plugin;

aeordb_query_plugin!(handle);

fn handle(ctx: PluginContext, request: PluginRequest) -> Result<PluginResponse, PluginError> {
    let function = request.metadata.get("function_name")
        .map(|s| s.as_str())
        .unwrap_or("echo");

    match function {
        "echo" => {
            // Echo back the request metadata
            PluginResponse::json(200, &serde_json::json!({
                "echo": true,
                "metadata": request.metadata,
                "body_len": request.arguments.len(),
            })).map_err(|e| PluginError::SerializationFailed(e.to_string()))
        }
        "read" => {
            let path = std::str::from_utf8(&request.arguments)
                .map_err(|e| PluginError::ExecutionFailed(e.to_string()))?;
            let file = ctx.read_file(path)?;
            PluginResponse::json(200, &serde_json::json!({
                "size": file.size,
                "content_type": file.content_type,
                "data_len": file.data.len(),
            })).map_err(|e| PluginError::SerializationFailed(e.to_string()))
        }
        "query" => {
            let results = ctx.query("/test")
                .field("name").contains("hello")
                .limit(5)
                .execute()?;
            PluginResponse::json(200, &serde_json::json!({
                "count": results.len(),
                "results": results,
            })).map_err(|e| PluginError::SerializationFailed(e.to_string()))
        }
        "list" => {
            let path = std::str::from_utf8(&request.arguments)
                .map_err(|e| PluginError::ExecutionFailed(e.to_string()))?;
            let entries = ctx.list_directory(path)?;
            PluginResponse::json(200, &serde_json::json!({
                "entries": entries,
            })).map_err(|e| PluginError::SerializationFailed(e.to_string()))
        }
        "write" => {
            ctx.write_file("/plugin-output/result.json", b"{\"written\":true}", "application/json")?;
            PluginResponse::json(201, &serde_json::json!({"ok": true}))
                .map_err(|e| PluginError::SerializationFailed(e.to_string()))
        }
        _ => Ok(PluginResponse::error(404, &format!("Unknown function: {}", function)))
    }
}
```

- [ ] **Step 2: Build the plugin to WASM**

```bash
cd aeordb-plugins/echo-plugin
cargo build --target wasm32-unknown-unknown --release
```

- [ ] **Step 3: Write E2E tests**

Create `aeordb-lib/spec/engine/wasm_query_e2e_spec.rs`:

Tests that load the echo-plugin WASM, deploy it, invoke various functions, and verify results:
- `test_echo_plugin_returns_metadata` — invoke "echo" function, verify metadata echoed
- `test_echo_plugin_reads_file` — store a file, invoke "read" function with path, verify size
- `test_echo_plugin_queries_database` — store indexed files, invoke "query" function, verify results
- `test_echo_plugin_lists_directory` — store files in /test/, invoke "list" with /test/, verify entries
- `test_echo_plugin_writes_file` — invoke "write", verify /plugin-output/result.json exists
- `test_echo_plugin_unknown_function_returns_404` — invoke nonexistent function, verify 404

Add `[[test]]` entry in Cargo.toml:
```toml
[[test]]
name = "wasm_query_e2e_spec"
path = "spec/engine/wasm_query_e2e_spec.rs"
```

- [ ] **Step 4: Run E2E tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test --test wasm_query_e2e_spec -- --test-threads=1`
Expected: All 6 tests pass

- [ ] **Step 5: Run full suite**

Run: `cd /home/wyatt/Projects/aeordb && cargo test -- --test-threads=4`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add aeordb-plugins/echo-plugin/ aeordb-lib/spec/engine/wasm_query_e2e_spec.rs aeordb-lib/Cargo.toml
git commit -m "WASM Query Phase 2.6: echo-plugin E2E tests — deploy, invoke, CRUD, query"
```

---

## Post-Implementation Checklist

- [ ] Update `.claude/TODO.md` — add "WASM Query Plugins (Phases 1+2)" with test count
- [ ] Update `.claude/DETAILS.md` — add context.rs, query_builder.rs, host functions to key files
- [ ] Remove old `HostFunctions` trait from `lib.rs` (replaced by `PluginContext`)
- [ ] Remove old `db_read`/`db_write`/`db_delete` stubs
- [ ] Run: `cargo test -- --test-threads=4` — all tests pass
- [ ] E2E: build echo-plugin, start server, deploy via HTTP, invoke via HTTP, verify response
