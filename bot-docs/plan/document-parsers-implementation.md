# Document Parsers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a configurable document parser pipeline so any file format can be parsed into JSON for indexing, with source path resolution, per-directory logging, and WASM plugin integration.

**Architecture:** Three-layer pipeline: Parser (WASM plugin, format→JSON) → Source Path Resolution (JSON traversal via array-of-segments) → Indexing Pipeline (existing converters+NVT). Configuration at `{dir}/.config/indexes.json` with `parser`, `source`, `type` (string or array), and `logging` fields. Errors logged to `{dir}/.logs/` when enabled.

**Tech Stack:** Rust, wasmi (WASM runtime), serde_json, base64, file-format crate

**Spec:** `bot-docs/plan/document-parsers.md`

---

### Task 1: Rename IndexFieldConfig — `field_name` → `name`, `converter_type` → `index_type`

The JSON key is `"type"` but `type` is a reserved keyword in Rust. Use `index_type` as the Rust field name with `#[serde(rename = "type")]` if using derive, or manual parsing with `item.get("type")`.

**Files:**
- Modify: `aeordb-lib/src/engine/index_config.rs`

- [ ] **Step 1: Update IndexFieldConfig struct**

```rust
#[derive(Debug, Clone)]
pub struct IndexFieldConfig {
    pub name: String,        // was field_name, JSON key: "name"
    pub index_type: String,  // was converter_type, JSON key: "type"
    pub source: Option<serde_json::Value>,  // NEW: array of segments or plugin object
    pub min: Option<f64>,
    pub max: Option<f64>,
}
```

- [ ] **Step 2: Update PathIndexConfig with new fields**

```rust
#[derive(Debug, Clone)]
pub struct PathIndexConfig {
    pub indexes: Vec<IndexFieldConfig>,
    pub parser: Option<String>,
    pub parser_memory_limit: Option<String>,
    pub logging: bool,
}
```

- [ ] **Step 3: Update `PathIndexConfig::deserialize` for new JSON keys**

Change the deserialization to read `"name"` and `"type"` from JSON. Handle `"type"` as string or array (when array, expand into multiple `IndexFieldConfig` entries — one per type). Read `"source"`, `"parser"`, `"parser_memory_limit"`, `"logging"` fields. Parse `"parser_memory_limit"` string like `"256mb"` into bytes.

Key changes in the parsing loop:
```rust
let name = item.get("name")
    .and_then(|v| v.as_str())
    .ok_or_else(|| EngineError::JsonParseError("Missing 'name' in index config".to_string()))?
    .to_string();

let types: Vec<String> = match item.get("type") {
    Some(serde_json::Value::String(s)) => vec![s.clone()],
    Some(serde_json::Value::Array(arr)) => {
        arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()
    }
    _ => return Err(EngineError::JsonParseError("Missing 'type' in index config".to_string())),
};

let source = item.get("source").cloned();

// Expand: one IndexFieldConfig per type
for index_type in types {
    indexes.push(IndexFieldConfig {
        name: name.clone(),
        index_type,
        source: source.clone(),
        min,
        max,
    });
}
```

Read top-level fields:
```rust
let parser = parsed.get("parser").and_then(|v| v.as_str()).map(String::from);
let parser_memory_limit = parsed.get("parser_memory_limit").and_then(|v| v.as_str()).map(String::from);
let logging = parsed.get("logging").and_then(|v| v.as_bool()).unwrap_or(false);
```

- [ ] **Step 4: Update `PathIndexConfig::serialize` for new field names**

Update the serializer to write `"name"` and `"type"` (not `"field"` and `"converter"`). Include `"source"` if present. Include top-level `"parser"`, `"parser_memory_limit"`, `"logging"` if set.

- [ ] **Step 5: Update `create_converter_from_config` to use `config.index_type`**

Change `config.converter_type.as_str()` → `config.index_type.as_str()` throughout the function.

- [ ] **Step 6: Update all callers of IndexFieldConfig fields**

Search for all uses of `.field_name` and `.converter_type` on `IndexFieldConfig` in the codebase. Key files:
- `aeordb-lib/src/engine/directory_ops.rs` (store_file_with_indexing, delete_file_with_indexing)
- `aeordb-lib/src/engine/query_engine.rs` (if any references)

Change `.field_name` → `.name` and `.converter_type` → `.index_type`.

- [ ] **Step 7: Build and fix any compilation errors**

Run: `cargo build 2>&1 | head -50`

- [ ] **Step 8: Commit**

```bash
git add aeordb-lib/src/engine/index_config.rs aeordb-lib/src/engine/directory_ops.rs
git commit -m "Rename IndexFieldConfig: field_name→name, converter_type→index_type, add source/parser/logging"
```

---

### Task 2: Update all test configs for new field names

~100 references across test files need updating. The JSON configs in tests use `"field_name"` and `"converter_type"` — these must change to `"name"` and `"type"`.

**Files:**
- Modify: All `aeordb-lib/spec/**/*_spec.rs` files that reference index configs

- [ ] **Step 1: Find and update all test config references**

Search for `field_name` and `converter_type` in all spec files. Replace:
- `"field_name"` → `"name"` (in JSON strings)
- `"converter_type"` → `"type"` (in JSON strings)
- `.field_name` → `.name` (in Rust struct access)
- `.converter_type` → `.index_type` (in Rust struct access)

Key files to check:
- `spec/engine/index_store_spec.rs`
- `spec/engine/query_engine_spec.rs`
- `spec/engine/multi_index_spec.rs`
- `spec/engine/trigram_spec.rs`
- `spec/engine/phonetic_spec.rs`
- `spec/engine/fuzzy_scoring_spec.rs`
- `spec/http/query_http_spec.rs`
- `spec/http/fuzzy_http_spec.rs`

- [ ] **Step 2: Build and run all tests**

Run: `cargo test 2>&1 | tail -5`
Expected: All 1,310 tests pass

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/spec/
git commit -m "Update all test configs: field_name→name, converter_type→type"
```

---

### Task 3: Source path resolution module

**Files:**
- Create: `aeordb-lib/src/engine/source_resolver.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`
- Create: `aeordb-lib/spec/engine/source_resolver_spec.rs`
- Modify: `aeordb-lib/Cargo.toml` (register test)

- [ ] **Step 1: Create `source_resolver.rs` with `resolve_source` function**

```rust
use crate::engine::json_parser::json_value_to_bytes;

/// Resolve a source path (array of segments) against a JSON value.
/// Returns the resolved value as bytes suitable for indexing.
///
/// Segments:
///   - String → object key lookup
///   - Integer → array index if current is array, else object key as string
///   - Other types → resolution failure (returns None)
pub fn resolve_source(json: &serde_json::Value, source: &[serde_json::Value]) -> Option<Vec<u8>> {
    let resolved = walk_path(json, source)?;
    Some(json_value_to_bytes(&resolved))
}

/// Walk a JSON value following the given path segments.
/// Returns the resolved JSON value, or None if any step fails.
pub fn walk_path(json: &serde_json::Value, segments: &[serde_json::Value]) -> Option<serde_json::Value> {
    let mut current = json;
    for segment in segments {
        match segment {
            serde_json::Value::String(key) => {
                current = current.get(key.as_str())?;
            }
            serde_json::Value::Number(n) => {
                let idx = n.as_u64()? as usize;
                if current.is_array() {
                    current = current.get(idx)?;
                } else {
                    // Try as string key on object
                    current = current.get(&idx.to_string())?;
                }
            }
            _ => return None,
        }
    }
    Some(current.clone())
}
```

Note: `json_value_to_bytes` in `json_parser.rs` is currently `fn` (private). It needs to be made `pub` so `source_resolver.rs` can use it. Read `json_parser.rs` and add `pub` to `fn json_value_to_bytes`.

- [ ] **Step 2: Register module in mod.rs**

Add `pub mod source_resolver;` and `pub use source_resolver::{resolve_source, walk_path};`

- [ ] **Step 3: Write tests**

Create `spec/engine/source_resolver_spec.rs` with tests:

```rust
// Test cases:
// 1. Simple string key: ["name"] on {"name": "Alice"} → "Alice" bytes
// 2. Nested keys: ["metadata", "title"] on {"metadata": {"title": "Report"}} → "Report"
// 3. Array index: ["items", 0] on {"items": ["a", "b"]} → "a"
// 4. Array index as object key: ["data", 0] on {"data": {"0": "val"}} → "val"
// 5. Deep nesting: ["a", "b", 2, "c"] on nested structure
// 6. Missing key: ["missing"] on {"name": "x"} → None
// 7. Missing nested: ["a", "b", "c"] where b doesn't exist → None
// 8. Array out of bounds: ["items", 99] on {"items": [1]} → None
// 9. Empty segments: [] on any value → returns the root
// 10. String with dots: ["metadata.title"] on {"metadata.title": "x"} → "x" (literal key)
// 11. Boolean segment: [true] → None (invalid segment type)
// 12. Null segment: [null] → None
// 13. Number value resolved: ["count"] on {"count": 42} → 42 as big-endian bytes
// 14. Nested array: ["matrix", 0, 1] on {"matrix": [[1,2],[3,4]]} → 2
// 15. Length as key: ["items", "length"] on {"items": {"length": 5}} → 5
```

Register in Cargo.toml:
```toml
[[test]]
name = "source_resolver_spec"
path = "spec/engine/source_resolver_spec.rs"
```

- [ ] **Step 4: Run tests**

Run: `cargo test --test source_resolver_spec 2>&1`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add aeordb-lib/src/engine/source_resolver.rs aeordb-lib/src/engine/mod.rs aeordb-lib/src/engine/json_parser.rs aeordb-lib/spec/engine/source_resolver_spec.rs aeordb-lib/Cargo.toml
git commit -m "Add source path resolution: walk JSON with array-of-segments syntax"
```

---

### Task 4: Recursive guard for system directories

Files stored at `.logs/*`, `.indexes/*`, or `.config/*` paths must NOT trigger the indexing pipeline. This prevents infinite recursion (logging triggers indexing which triggers logging...).

**Files:**
- Modify: `aeordb-lib/src/engine/directory_ops.rs`
- Create: `aeordb-lib/spec/engine/recursive_guard_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Add guard function**

In `directory_ops.rs`, add a helper:

```rust
/// Check if a path is a system directory that should not trigger indexing.
/// Returns true for .logs/*, .indexes/*, .config/* paths.
fn is_system_path(path: &str) -> bool {
    let normalized = normalize_path(path);
    let segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    segments.iter().any(|s| *s == ".logs" || *s == ".indexes" || *s == ".config")
}
```

- [ ] **Step 2: Add guard check at top of store_file_with_indexing**

After storing the file but before reading index config:

```rust
// Guard: skip indexing for system directories
if is_system_path(path) {
    return Ok(file_record);
}
```

Actually — the file must still be stored. The guard only skips the indexing part, not the store. Place the guard after `store_file_internal` returns the `file_record`, before reading the index config.

- [ ] **Step 3: Write tests**

```rust
// 1. test_system_path_logs → is_system_path("/people/.logs/system/indexing.log") == true
// 2. test_system_path_indexes → is_system_path("/people/.indexes/name.trigram.idx") == true
// 3. test_system_path_config → is_system_path("/people/.config/indexes.json") == true
// 4. test_normal_path → is_system_path("/people/smith.json") == false
// 5. test_root_path → is_system_path("/data.json") == false
// 6. test_nested_system → is_system_path("/a/b/.logs/c.log") == true
// 7. test_dotfile_not_system → is_system_path("/people/.hidden/file.txt") == false
```

- [ ] **Step 4: Run all tests**

Run: `cargo test 2>&1 | tail -5`

- [ ] **Step 5: Commit**

```bash
git add aeordb-lib/src/engine/directory_ops.rs aeordb-lib/spec/engine/recursive_guard_spec.rs aeordb-lib/Cargo.toml
git commit -m "Add recursive guard: skip indexing for .logs/*, .indexes/*, .config/* paths"
```

---

### Task 5: Extract IndexingPipeline from store_file_with_indexing

Decompose the growing `store_file_with_indexing` method into a focused `IndexingPipeline` struct.

**Files:**
- Create: `aeordb-lib/src/engine/indexing_pipeline.rs`
- Modify: `aeordb-lib/src/engine/directory_ops.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`

- [ ] **Step 1: Create `indexing_pipeline.rs`**

Move the indexing logic from `store_file_with_indexing` into a new struct:

```rust
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::index_config::{PathIndexConfig, IndexFieldConfig, create_converter_from_config};
use crate::engine::index_store::IndexManager;
use crate::engine::json_parser::parse_json_fields;
use crate::engine::source_resolver::resolve_source;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::path_utils::{normalize_path, parent_path, file_name};

pub struct IndexingPipeline<'a> {
    engine: &'a StorageEngine,
}

impl<'a> IndexingPipeline<'a> {
    pub fn new(engine: &'a StorageEngine) -> Self {
        IndexingPipeline { engine }
    }

    /// Run the indexing pipeline for a stored file.
    /// This is called AFTER the file has been stored.
    pub fn run(
        &self,
        path: &str,
        data: &[u8],
        content_type: Option<&str>,
    ) -> EngineResult<()> {
        let normalized = normalize_path(path);
        let parent = parent_path(&normalized).unwrap_or_else(|| "/".to_string());

        // Load config
        let config = match self.load_config(&parent)? {
            Some(c) => c,
            None => return Ok(()), // no config, no indexing
        };

        // Determine the JSON to index from
        let json_data = self.get_json_data(data, &config, path, content_type)?;

        // Index each configured field
        let algo = self.engine.hash_algo();
        let file_key = crate::engine::directory_ops::file_path_hash(&normalized, &algo)?;
        let index_manager = IndexManager::new(self.engine);

        for field_config in &config.indexes {
            if let Err(e) = self.index_field(&field_config, &json_data, &file_key, &parent, &index_manager) {
                if config.logging {
                    self.log_system(&parent, "indexing.log",
                        &format!("field '{}' indexing failed for {}: {}", field_config.name, path, e));
                }
            }
        }

        Ok(())
    }

    fn load_config(&self, parent: &str) -> EngineResult<Option<PathIndexConfig>> {
        let config_path = if parent.ends_with('/') {
            format!("{}.config/indexes.json", parent)
        } else {
            format!("{}/.config/indexes.json", parent)
        };

        let ops = DirectoryOps::new(self.engine);
        match ops.read_file(&config_path) {
            Ok(config_data) => PathIndexConfig::deserialize(&config_data).map(Some),
            Err(EngineError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn get_json_data(
        &self,
        data: &[u8],
        config: &PathIndexConfig,
        path: &str,
        content_type: Option<&str>,
    ) -> EngineResult<serde_json::Value> {
        // If parser configured, invoke it (Phase 4 — for now, skip)
        // If no parser and content is JSON, parse directly
        let text = std::str::from_utf8(data).map_err(|e| {
            EngineError::JsonParseError(format!("Invalid UTF-8: {}", e))
        })?;
        serde_json::from_str(text).map_err(|e| {
            EngineError::JsonParseError(format!("Invalid JSON: {}", e))
        })
    }

    fn index_field(
        &self,
        field_config: &IndexFieldConfig,
        json_data: &serde_json::Value,
        file_key: &[u8],
        parent: &str,
        index_manager: &IndexManager,
    ) -> EngineResult<()> {
        // Resolve source path
        let source_segments = field_config.source.as_ref()
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_else(|| vec![serde_json::Value::String(field_config.name.clone())]);

        let field_value = match resolve_source(json_data, &source_segments) {
            Some(bytes) => bytes,
            None => return Ok(()), // source not found, skip silently
        };

        // Load or create index
        let converter = create_converter_from_config(field_config)?;
        let strategy = converter.strategy().to_string();
        let mut index = match index_manager.load_index_by_strategy(parent, &field_config.name, &strategy)? {
            Some(idx) => idx,
            None => index_manager.create_index(parent, &field_config.name, converter)?,
        };

        // Remove old entries, insert new
        index.remove(file_key);
        index.insert_expanded(&field_value, file_key.to_vec());
        index_manager.save_index(parent, &index)?;

        Ok(())
    }

    /// Write a log entry to .logs/system/{log_name}
    fn log_system(&self, parent: &str, log_name: &str, message: &str) {
        let log_path = if parent.ends_with('/') {
            format!("{}.logs/system/{}", parent, log_name)
        } else {
            format!("{}/.logs/system/{}", parent, log_name)
        };

        let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        let entry = format!("{} WARN  {}\n", timestamp, message);

        let ops = DirectoryOps::new(self.engine);

        // Read existing log, append, write back
        let existing = ops.read_file(&log_path).unwrap_or_default();
        let mut combined = existing;
        combined.extend_from_slice(entry.as_bytes());

        // Store — ignore errors (don't fail indexing because logging failed)
        let _ = ops.store_file(&log_path, &combined, Some("text/plain"));
    }
}
```

- [ ] **Step 2: Simplify store_file_with_indexing to delegate**

In `directory_ops.rs`, replace the indexing logic in `store_file_with_indexing` with:

```rust
pub fn store_file_with_indexing(
    &self,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
) -> EngineResult<FileRecord> {
    // Compression detection (unchanged)
    // ... existing compression code ...

    let file_record = self.store_file_internal(path, data, content_type, compression_algo)?;

    // Guard: skip indexing for system directories
    if is_system_path(path) {
        return Ok(file_record);
    }

    // Delegate to indexing pipeline
    let pipeline = crate::engine::indexing_pipeline::IndexingPipeline::new(self.engine);
    let _ = pipeline.run(path, data, content_type); // Don't fail the store if indexing fails

    Ok(file_record)
}
```

- [ ] **Step 3: Register module, make file_path_hash pub**

Add `pub mod indexing_pipeline;` to mod.rs. The `file_path_hash` function in directory_ops.rs needs to be `pub` (or `pub(crate)`) so indexing_pipeline.rs can use it. Check if it's already public; if not, make it `pub(crate)`.

- [ ] **Step 4: Build and run all tests**

Run: `cargo test 2>&1 | tail -5`
Expected: All tests pass (behavior unchanged, just restructured)

- [ ] **Step 5: Commit**

```bash
git add aeordb-lib/src/engine/indexing_pipeline.rs aeordb-lib/src/engine/directory_ops.rs aeordb-lib/src/engine/mod.rs
git commit -m "Extract IndexingPipeline from store_file_with_indexing"
```

---

### Task 6: Per-directory `.logs/` system

**Files:**
- Modify: `aeordb-lib/src/engine/indexing_pipeline.rs` (log_system already stubbed)
- Create: `aeordb-lib/spec/engine/logging_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Write tests for logging behavior**

```rust
// 1. test_logging_disabled_no_log_created — store file with logging=false, verify .logs/ doesn't exist
// 2. test_logging_enabled_creates_log — store file with bad source path, logging=true, verify .logs/system/indexing.log exists
// 3. test_log_contains_error_message — verify log content has timestamp, level, and field name
// 4. test_log_appends — trigger two errors, verify both appear in the log
// 5. test_log_stored_in_database — verify log is readable via DirectoryOps::read_file
// 6. test_log_not_indexed — verify storing to .logs/ doesn't trigger indexing (recursive guard)
```

- [ ] **Step 2: Run tests**

Run: `cargo test --test logging_spec 2>&1`

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/spec/engine/logging_spec.rs aeordb-lib/Cargo.toml aeordb-lib/src/engine/indexing_pipeline.rs
git commit -m "Add per-directory .logs/ system with opt-in indexing error logging"
```

---

### Task 7: Integrate source path resolution into pipeline

Wire `resolve_source` into the `IndexingPipeline` so that the `"source"` field in index configs is used instead of flat JSON key lookup.

**Files:**
- Modify: `aeordb-lib/src/engine/indexing_pipeline.rs`
- Create: `aeordb-lib/spec/engine/pipeline_source_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Write integration tests**

```rust
// Setup: create engine, store config with source paths, store JSON files, verify indexes
// 1. test_source_simple_key — {"name":"title","type":"string"} with no source, extracts "title" key
// 2. test_source_nested — {"name":"title","source":["metadata","title"],"type":"string"}, JSON has nested metadata.title
// 3. test_source_array_index — {"name":"first","source":["items",0],"type":"string"}, JSON has items array
// 4. test_source_missing_path — source points to nonexistent key, field is skipped, no error
// 5. test_source_missing_path_with_logging — same but logging=true, verify .logs/system/indexing.log created
// 6. test_type_array_expansion — {"name":"title","type":["string","trigram"]}, verify both .idx files created
// 7. test_multiple_fields_different_sources — two fields with different source paths
// 8. test_source_integer_key_on_object — source [data, 0] where data is {"0": "val"}
```

- [ ] **Step 2: Verify existing indexing tests still pass**

Run: `cargo test 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/spec/engine/pipeline_source_spec.rs aeordb-lib/Cargo.toml
git commit -m "Integrate source path resolution into indexing pipeline with tests"
```

---

### Task 8: WASM parser invocation with configurable memory

**Files:**
- Modify: `aeordb-lib/src/plugins/plugin_manager.rs`
- Modify: `aeordb-lib/src/engine/indexing_pipeline.rs`
- Create: `aeordb-lib/spec/engine/parser_plugin_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Add `invoke_wasm_plugin_with_limits` to PluginManager**

```rust
pub fn invoke_wasm_plugin_with_limits(
    &self,
    path: &str,
    request_bytes: &[u8],
    memory_limit_bytes: usize,
) -> Result<Vec<u8>, PluginManagerError> {
    // Same as invoke_wasm_plugin but uses WasmPluginRuntime::with_limits
    let record = self.get_plugin(path)?
        .ok_or_else(|| PluginManagerError::NotFound(path.to_string()))?;

    if record.plugin_type != PluginType::Wasm {
        return Err(PluginManagerError::InvalidPlugin(format!(
            "plugin at '{}' is not a WASM plugin", path
        )));
    }

    let runtime = WasmPluginRuntime::with_limits(
        &record.wasm_bytes,
        memory_limit_bytes,
        DEFAULT_FUEL_LIMIT,  // use constant from wasm_runtime
    ).map_err(|e| PluginManagerError::ExecutionFailed(format!("load failed: {}", e)))?;

    runtime.call_handle(request_bytes)
        .map_err(|e| PluginManagerError::ExecutionFailed(format!("execution failed: {}", e)))
}
```

- [ ] **Step 2: Add `parse_memory_limit` helper to indexing_pipeline.rs**

```rust
fn parse_memory_limit(limit_str: &str) -> usize {
    let s = limit_str.trim().to_lowercase();
    if let Some(mb) = s.strip_suffix("mb") {
        mb.trim().parse::<usize>().unwrap_or(256) * 1024 * 1024
    } else if let Some(gb) = s.strip_suffix("gb") {
        gb.trim().parse::<usize>().unwrap_or(1) * 1024 * 1024 * 1024
    } else if let Some(kb) = s.strip_suffix("kb") {
        kb.trim().parse::<usize>().unwrap_or(256 * 1024) * 1024
    } else {
        s.parse::<usize>().unwrap_or(256 * 1024 * 1024) // default 256MB
    }
}
```

- [ ] **Step 3: Add parser envelope builder**

```rust
use crate::engine::file_record::FileRecord;

fn build_parser_envelope(
    data: &[u8],
    path: &str,
    content_type: Option<&str>,
    file_record: &FileRecord,
    hash_algo: &str,
) -> serde_json::Value {
    let filename = crate::engine::path_utils::file_name(path).unwrap_or_default();
    serde_json::json!({
        "data": base64::engine::general_purpose::STANDARD.encode(data),
        "meta": {
            "filename": filename,
            "path": path,
            "content_type": content_type.unwrap_or("application/octet-stream"),
            "size": data.len(),
            "hash": hex::encode(&file_record.chunk_hashes.first().unwrap_or(&vec![])),
            "hash_algorithm": hash_algo,
            "created_at": file_record.created_at,
            "updated_at": file_record.updated_at,
        }
    })
}
```

Note: `base64` crate needs to be added to dependencies if not already present. Check `Cargo.toml`.

- [ ] **Step 4: Wire parser invocation into `get_json_data`**

Update `IndexingPipeline::get_json_data` to invoke the parser if configured:

```rust
fn get_json_data(
    &self,
    data: &[u8],
    config: &PathIndexConfig,
    path: &str,
    content_type: Option<&str>,
    file_record: &FileRecord,
    plugin_manager: Option<&PluginManager>,
) -> EngineResult<serde_json::Value> {
    if let Some(parser_name) = &config.parser {
        // Invoke parser plugin
        let pm = plugin_manager.ok_or_else(|| {
            EngineError::NotFound("Plugin manager required for parser invocation".to_string())
        })?;

        let memory_limit = config.parser_memory_limit.as_deref()
            .map(parse_memory_limit)
            .unwrap_or(256 * 1024 * 1024);

        let hash_algo = format!("{:?}", self.engine.hash_algo());
        let envelope = build_parser_envelope(data, path, content_type, file_record, &hash_algo);
        let envelope_bytes = serde_json::to_vec(&envelope).map_err(|e| {
            EngineError::JsonParseError(format!("Failed to serialize parser envelope: {}", e))
        })?;

        let parser_path = parser_name.to_string(); // plugin path
        let output = pm.invoke_wasm_plugin_with_limits(&parser_path, &envelope_bytes, memory_limit)
            .map_err(|e| EngineError::NotFound(format!("Parser '{}' failed: {}", parser_name, e)))?;

        // Validate output is JSON object
        let text = std::str::from_utf8(&output).map_err(|_| {
            EngineError::JsonParseError("Parser returned invalid UTF-8".to_string())
        })?;
        let parsed: serde_json::Value = serde_json::from_str(text).map_err(|e| {
            EngineError::JsonParseError(format!("Parser returned invalid JSON: {}", e))
        })?;
        if !parsed.is_object() {
            return Err(EngineError::JsonParseError("Parser must return a JSON object".to_string()));
        }
        Ok(parsed)
    } else {
        // No parser — try raw data as JSON
        let text = std::str::from_utf8(data).map_err(|e| {
            EngineError::JsonParseError(format!("Invalid UTF-8: {}", e))
        })?;
        serde_json::from_str(text).map_err(|e| {
            EngineError::JsonParseError(format!("Invalid JSON: {}", e))
        })
    }
}
```

This requires updating `IndexingPipeline` to accept an optional `&PluginManager`. Update the `new()` constructor and `run()` method signature:

```rust
pub struct IndexingPipeline<'a> {
    engine: &'a StorageEngine,
    plugin_manager: Option<&'a PluginManager>,
}

impl<'a> IndexingPipeline<'a> {
    pub fn new(engine: &'a StorageEngine, plugin_manager: Option<&'a PluginManager>) -> Self {
        IndexingPipeline { engine, plugin_manager }
    }
}
```

Update the caller in `directory_ops.rs` too — it needs to pass `None` for now (plugin manager isn't available at that level yet without wiring through AppState).

- [ ] **Step 5: Add `base64` dependency if needed**

Check `Cargo.toml`. If `base64` is not already a dependency, add it:
```toml
base64 = "0.22"
```

- [ ] **Step 6: Write tests**

```rust
// 1. test_parse_memory_limit_mb — "256mb" → 268435456
// 2. test_parse_memory_limit_gb — "1gb" → 1073741824
// 3. test_parse_memory_limit_default — "invalid" → 268435456 (256MB)
// 4. test_parser_envelope_structure — verify envelope has data (base64) and meta fields
// 5. test_parser_envelope_filename — verify filename extracted from path
// 6. test_parser_not_configured_uses_raw_json — no parser, raw JSON data used
// 7. test_parser_not_found_logs_error — parser name in config but not deployed, logged
// 8. test_non_json_without_parser_fails_gracefully — binary data, no parser, indexing skipped
```

- [ ] **Step 7: Build and run tests**

Run: `cargo test 2>&1 | tail -5`

- [ ] **Step 8: Commit**

```bash
git add aeordb-lib/src/engine/indexing_pipeline.rs aeordb-lib/src/plugins/plugin_manager.rs aeordb-lib/Cargo.toml aeordb-lib/spec/engine/parser_plugin_spec.rs
git commit -m "Add parser plugin invocation with configurable memory and metadata envelope"
```

---

### Task 9: Content-type fallback + global parser registry

**Files:**
- Modify: `aeordb-lib/src/engine/indexing_pipeline.rs`
- Create: `aeordb-lib/spec/engine/parser_registry_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Add parser registry lookup to pipeline**

```rust
/// Look up a parser name from the global registry at /.config/parsers.json
fn lookup_parser_by_content_type(
    &self,
    content_type: &str,
) -> EngineResult<Option<String>> {
    let ops = DirectoryOps::new(self.engine);
    match ops.read_file("/.config/parsers.json") {
        Ok(data) => {
            let text = std::str::from_utf8(&data).unwrap_or("{}");
            let registry: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
            Ok(registry.get(content_type)
                .and_then(|v| v.as_str())
                .map(String::from))
        }
        Err(EngineError::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}
```

- [ ] **Step 2: Wire into `get_json_data` — check config parser first, then registry**

Update the parser selection logic:
1. If `config.parser` is set → use it
2. Else if content_type is available → check registry
3. Else if content_type is `application/json` or missing → use raw data as JSON
4. Else → no parser, no indexing (return empty JSON object or error)

- [ ] **Step 3: Write tests**

```rust
// 1. test_registry_lookup_found — store /.config/parsers.json, lookup "application/pdf" → "pdf-parser"
// 2. test_registry_lookup_not_found — lookup unregistered type → None
// 3. test_registry_not_exists — no /.config/parsers.json file → None
// 4. test_explicit_parser_overrides_registry — config has parser, registry has different → config wins
// 5. test_json_content_type_skips_registry — application/json → raw data, no registry lookup
```

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/engine/indexing_pipeline.rs aeordb-lib/spec/engine/parser_registry_spec.rs aeordb-lib/Cargo.toml
git commit -m "Add content-type parser registry fallback at /.config/parsers.json"
```

---

### Task 10: Plugin mapper source

**Files:**
- Modify: `aeordb-lib/src/engine/indexing_pipeline.rs`
- Create: `aeordb-lib/spec/engine/plugin_mapper_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Handle plugin mapper in `index_field`**

In `IndexingPipeline::index_field`, check if `source` is an object with `"plugin"` key:

```rust
fn index_field(&self, ...) -> EngineResult<()> {
    let field_value = if let Some(source) = &field_config.source {
        if let Some(obj) = source.as_object() {
            if let Some(plugin_name) = obj.get("plugin").and_then(|v| v.as_str()) {
                // Plugin mapper
                let args = obj.get("args").cloned().unwrap_or(serde_json::Value::Null);
                let mapper_input = serde_json::json!({
                    "data": json_data,
                    "args": args,
                });
                let input_bytes = serde_json::to_vec(&mapper_input).map_err(|e| {
                    EngineError::JsonParseError(format!("Mapper input serialization failed: {}", e))
                })?;

                let pm = self.plugin_manager.ok_or_else(|| {
                    EngineError::NotFound("Plugin manager required for mapper".to_string())
                })?;

                pm.invoke_wasm_plugin(plugin_name, &input_bytes)
                    .map_err(|e| EngineError::NotFound(format!("Mapper '{}' failed: {}", plugin_name, e)))?
            } else {
                return Ok(()); // invalid source object
            }
        } else if let Some(segments) = source.as_array() {
            // Array path resolution
            match resolve_source(json_data, segments) {
                Some(bytes) => bytes,
                None => return Ok(()),
            }
        } else {
            return Ok(());
        }
    } else {
        // Default: use field name as key
        let default_source = vec![serde_json::Value::String(field_config.name.clone())];
        match resolve_source(json_data, &default_source) {
            Some(bytes) => bytes,
            None => return Ok(()),
        }
    };

    // ... rest of indexing (load/create index, remove old, insert_expanded, save)
}
```

- [ ] **Step 2: Write tests**

```rust
// 1. test_plugin_mapper_invocation — source: {"plugin": "test-mapper"}, verify plugin receives data+args
// 2. test_plugin_mapper_with_args — source: {"plugin": "m", "args": {"mode": "summary"}}, verify args passed
// 3. test_plugin_mapper_no_args — source: {"plugin": "m"}, verify args is null
// 4. test_plugin_mapper_not_found — plugin not deployed, field skipped, logged if enabled
// 5. test_array_source_still_works — verify array source isn't broken by the new plugin path
// 6. test_default_source_still_works — verify no source still defaults to field name
```

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/src/engine/indexing_pipeline.rs aeordb-lib/spec/engine/plugin_mapper_spec.rs aeordb-lib/Cargo.toml
git commit -m "Add plugin mapper source: invoke WASM plugin with data+args for field extraction"
```

---

### Task 11: WASM `log` host function

**Files:**
- Modify: `aeordb-lib/src/plugins/wasm_runtime.rs`
- Create: `aeordb-lib/spec/plugins/log_host_function_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Add ParserContext to HostState**

```rust
pub struct ParserContext {
    pub directory: String,
    pub plugin_name: String,
    pub logging_enabled: bool,
    pub log_entries: Vec<String>,  // collected during execution, flushed after
}

struct HostState {
    memory: Option<Memory>,
    parser_context: Option<ParserContext>,
}
```

- [ ] **Step 2: Add `call_handle_with_context` method**

```rust
pub fn call_handle_with_context(
    &self,
    request_bytes: &[u8],
    context: ParserContext,
) -> Result<(Vec<u8>, Vec<String>), WasmRuntimeError> {
    // Same as call_handle but passes context in HostState
    // Returns (response_bytes, collected_log_entries)
    // ... similar to call_handle but with context.log_entries collected
}
```

- [ ] **Step 3: Implement `log` host function in `register_host_functions`**

```rust
linker.func_wrap(
    "aeordb",
    "log",
    |mut caller: Caller<'_, HostState>, level_ptr: i32, level_len: i32, msg_ptr: i32, msg_len: i32| {
        let memory = caller.data().memory.expect("memory not set");
        let level = read_string_from_memory(&memory, &caller, level_ptr as usize, level_len as usize);
        let message = read_string_from_memory(&memory, &caller, msg_ptr as usize, msg_len as usize);

        if let (Ok(level), Ok(msg)) = (level, message) {
            if let Some(ref mut ctx) = caller.data_mut().parser_context {
                if ctx.logging_enabled {
                    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
                    ctx.log_entries.push(format!("{} {} {}", timestamp, level, msg));
                }
            }
        }
    },
).map_err(|e| WasmRuntimeError::InstantiationFailed(e.to_string()))?;
```

- [ ] **Step 4: Flush logs after parser invocation in IndexingPipeline**

After calling the parser, collect log entries from the context and write them to `.logs/plugins/{plugin_name}.log`:

```rust
// After parser invocation returns
for entry in log_entries {
    self.log_plugin(parent, parser_name, &entry);
}
```

- [ ] **Step 5: Write tests**

```rust
// 1. test_log_host_function_collects_entries — WASM calls log, entries collected
// 2. test_log_disabled_no_entries — logging=false, log calls are no-op
// 3. test_log_entries_flushed_to_file — after execution, entries written to .logs/plugins/
// 4. test_call_handle_without_context — existing call_handle still works (no context)
```

- [ ] **Step 6: Commit**

```bash
git add aeordb-lib/src/plugins/wasm_runtime.rs aeordb-lib/src/engine/indexing_pipeline.rs aeordb-lib/spec/plugins/log_host_function_spec.rs aeordb-lib/Cargo.toml
git commit -m "Implement WASM log host function: plugins write to .logs/plugins/{name}.log"
```

---

### Task 12: Wire PluginManager into indexing pipeline via server

The `IndexingPipeline` needs access to `PluginManager` for parser and mapper invocations. Currently, `store_file_with_indexing` in `DirectoryOps` doesn't have access to it. We need to thread it through.

**Files:**
- Modify: `aeordb-lib/src/engine/directory_ops.rs`
- Modify: `aeordb-lib/src/server/engine_routes.rs`

- [ ] **Step 1: Add PluginManager-aware indexing method**

Add a new method to DirectoryOps that accepts an optional PluginManager:

```rust
pub fn store_file_with_full_pipeline(
    &self,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
    plugin_manager: Option<&PluginManager>,
) -> EngineResult<FileRecord> {
    // ... compression detection (unchanged) ...
    let file_record = self.store_file_internal(path, data, content_type, compression_algo)?;

    if is_system_path(path) {
        return Ok(file_record);
    }

    let pipeline = IndexingPipeline::new(self.engine, plugin_manager);
    let _ = pipeline.run(path, data, content_type);

    Ok(file_record)
}
```

Keep `store_file_with_indexing` as-is for backward compat (passes `None` for plugin_manager).

- [ ] **Step 2: Update engine_routes to use full pipeline**

In `engine_store_file`, change the call to use the new method and pass `state.plugin_manager`:

```rust
let file_record = ops.store_file_with_full_pipeline(
    &path, &body, content_type, Some(&state.plugin_manager)
)?;
```

- [ ] **Step 3: Build and run all tests**

Run: `cargo test 2>&1 | tail -5`

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/engine/directory_ops.rs aeordb-lib/src/server/engine_routes.rs
git commit -m "Wire PluginManager into indexing pipeline via store_file_with_full_pipeline"
```

---

### Task 13: E2E test with a simple WASM test parser

Build a minimal WASM parser that extracts basic metadata from plain text files, then test the full pipeline end-to-end.

**Files:**
- Create: test WASM binary (build from a minimal Rust crate)
- Create: `aeordb-lib/spec/engine/e2e_parser_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Build a minimal test parser WASM binary**

Create a simple WASM parser that reads the base64-decoded data from the parser envelope and returns JSON with basic text stats:

The test binary can be pre-compiled and included as `&[u8]` in the test, OR built at test time. Pre-compiled is simpler. Use the existing test WASM patterns from `spec/plugins/wasm_runtime_spec.rs` — read that file to see how test WASM binaries are handled.

The parser should: decode base64 data, count bytes/lines/words, extract filename from meta, return JSON:
```json
{"line_count": 10, "word_count": 50, "byte_count": 500, "filename": "test.txt"}
```

- [ ] **Step 2: Write E2E test**

```rust
// 1. Deploy test parser plugin
// 2. Store config: parser="test-parser", indexes with source paths into parser output
// 3. Store a text file
// 4. Verify indexes were created with correct values
// 5. Query via the index — verify results
```

- [ ] **Step 3: Test full pipeline with content-type fallback**

```rust
// 1. Store global parser registry at /.config/parsers.json with "text/plain" → "test-parser"
// 2. Store a text file with Content-Type: text/plain (no explicit parser in dir config)
// 3. Verify parser was selected via registry and indexes created
```

- [ ] **Step 4: Test error paths**

```rust
// 1. Parser not deployed → file stored, no indexes, log if enabled
// 2. Parser returns non-JSON → file stored, no indexes, logged
// 3. Source path not found → field skipped, other fields indexed, logged
// 4. Plugin mapper not deployed → field skipped, logged
```

- [ ] **Step 5: Run full test suite**

Run: `cargo test 2>&1 | tail -5`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add aeordb-lib/spec/engine/e2e_parser_spec.rs aeordb-lib/Cargo.toml
git commit -m "E2E test: full parser pipeline with WASM test parser"
```

---

### Task 14: Update HTTP query endpoint for `type` rename

The HTTP query endpoint in `engine_routes.rs` uses `parse_single_field_query` which builds `QueryOp` from JSON. The query input format doesn't use `field_name`/`converter_type` — it uses `"field"` and `"op"`, so this may not need changes. Verify and update if needed.

**Files:**
- Modify: `aeordb-lib/src/server/engine_routes.rs` (if needed)

- [ ] **Step 1: Verify HTTP query format is unaffected**

The query POST body uses `{"field": "name", "op": "contains", "value": "..."}` — this is separate from the index config format. The `"field"` key in queries maps to `FieldQuery.field_name` in the query engine, which is a different struct from `IndexFieldConfig`. Verify no changes needed.

- [ ] **Step 2: Run HTTP test suite**

Run: `cargo test --test fuzzy_http_spec --test query_http_spec 2>&1`

- [ ] **Step 3: Commit if changes were needed**

---

### Task 15: Final cleanup — update DETAILS.md, TODO.md

**Files:**
- Modify: `.claude/DETAILS.md`
- Modify: `.claude/TODO.md`

- [ ] **Step 1: Update DETAILS.md**

Add key files:
- `aeordb-lib/src/engine/indexing_pipeline.rs` — IndexingPipeline, parser invocation, source resolution
- `aeordb-lib/src/engine/source_resolver.rs` — resolve_source, walk_path

Update test count.

- [ ] **Step 2: Update TODO.md**

Mark document parsers as complete with test count.

- [ ] **Step 3: Run full test suite one final time**

Run: `cargo test 2>&1 | grep "test result" | awk '{sum += $4; fail += $6} END {print "Passed:", sum, "Failed:", fail}'`

- [ ] **Step 4: Commit**

```bash
git add .claude/DETAILS.md .claude/TODO.md
git commit -m "Update tracking docs: document parsers complete"
```
