# Global Search + Default Indexes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Default NVT-backed indexes on every file's metadata (@filename, @hash, @created_at, @updated_at, @size, @content_type), plus a global search endpoint that fans out across all indexed directories.

**Architecture:** The indexing pipeline gains @-field support (extracts from FileRecord instead of parsed JSON). On first boot, a default index config is written at `/` with `glob: "**/*"`, triggering a full reindex. A new `POST /files/search` endpoint discovers all indexed directories and fans out queries, merging results by score. Internal paths (.config, .indexes, .logs) are excluded from indexing.

**Tech Stack:** Rust (axum, AeorDB engine)

---

### Task 1: @-Field Support in Indexing Pipeline

**Files:**
- Modify: `aeordb-lib/src/engine/indexing_pipeline.rs`
- Test: `aeordb-lib/spec/engine/pipeline_spec.rs`

- [ ] **Step 1: Add @-field extraction in `index_field`**

In `indexing_pipeline.rs`, find the `index_field` method (around line 290). Before the existing source resolution logic (the `resolve_sources` call), add a check for @-prefixed field names. When detected, load the FileRecord from the engine and extract the metadata value directly instead of parsing the file content.

The pipeline already has `self.engine` and the file's `path`. Use `file_path_hash` + `get_entry` to load the FileRecord:

```rust
// At the top of index_field, before source resolution:
if field_config.name.starts_with('@') {
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();
    let file_key = crate::engine::directory_ops::file_path_hash(path, &algo)?;
    let file_record = match self.engine.get_entry(&file_key)? {
        Some((header, _key, value)) => {
            crate::engine::file_record::FileRecord::deserialize(&value, hash_length, header.entry_version)?
        }
        None => return Ok(()),
    };

    let field_bytes: Vec<Vec<u8>> = match field_config.name.as_str() {
        "@filename" => {
            let name = crate::engine::path_utils::file_name(path).unwrap_or("");
            vec![name.as_bytes().to_vec()]
        }
        "@hash" => {
            if let Some(first_chunk) = file_record.chunk_hashes.first() {
                vec![hex::encode(first_chunk).into_bytes()]
            } else {
                vec![]
            }
        }
        "@created_at" => vec![file_record.created_at.to_be_bytes().to_vec()],
        "@updated_at" => vec![file_record.updated_at.to_be_bytes().to_vec()],
        "@size" => vec![file_record.total_size.to_be_bytes().to_vec()],
        "@content_type" => {
            if let Some(ref ct) = file_record.content_type {
                vec![ct.as_bytes().to_vec()]
            } else {
                vec![]
            }
        }
        _ => return Ok(()), // Unknown @ field
    };

    if field_bytes.is_empty() { return Ok(()); }

    // Load or create the index, remove old entries, insert new
    // (use the same index insertion logic as below, with config_dir)
    // ... continue with existing insert logic using field_bytes ...
    // Return early — don't fall through to JSON source resolution
}
```

Factor the index load/insert/save logic into a helper so both the @-field path and the JSON source path share it. Something like:

```rust
fn insert_field_values(
    &self,
    config_dir: &str,
    field_config: &IndexFieldConfig,
    file_key: &[u8],
    values: &[Vec<u8>],
) -> EngineResult<()>
```

- [ ] **Step 2: Also skip internal paths in pipeline**

In the `run` method (around line 115), add an early return for internal paths:

```rust
if crate::engine::directory_ops::is_internal_path(path) {
    return Ok(());
}
```

Note: `is_internal_path` is currently `fn` (private). Make it `pub fn` in `directory_ops.rs`.

- [ ] **Step 3: Write tests**

Add tests to `pipeline_spec.rs`:

```rust
#[test]
fn test_at_filename_field_indexed() {
    // Store config at /test/ with glob and @filename trigram index
    // Store a file at /test/hello.json
    // Run pipeline
    // Verify @filename index exists at /test/.indexes/ with entry for "hello.json"
}

#[test]
fn test_at_created_at_field_indexed() {
    // Same pattern but with @created_at timestamp index
}

#[test]
fn test_internal_paths_skipped() {
    // Store a file at /test/.config/indexes.json
    // Run pipeline — should be a no-op
}
```

- [ ] **Step 4: Build and test**

```bash
cargo build --release 2>&1 | tail -5
cargo test --release -p aeordb --test pipeline_spec 2>&1 | tail -15
```

- [ ] **Step 5: Commit**

```bash
git commit -am "feat: @-field indexing support in pipeline (extracts from FileRecord metadata)"
```

---

### Task 2: @hash Virtual Field in Query Engine

**Files:**
- Modify: `aeordb-lib/src/engine/query_engine.rs`

- [ ] **Step 1: Add @hash to virtual field evaluation**

In `query_engine.rs`, find `evaluate_virtual_field_query` (around line 1065). Add `@hash` to the supported field list and the match arm:

```rust
"@hash" => {
    let hash_hex = if let Some(first_chunk) = file_record.chunk_hashes.first() {
        hex::encode(first_chunk)
    } else {
        String::new()
    };
    // Evaluate using hash_hex as the string value
}
```

Also add `@hash` to the `sort_results` match arm (around line 881).

- [ ] **Step 2: Build and verify**

```bash
cargo build --release 2>&1 | tail -5
```

- [ ] **Step 3: Commit**

```bash
git commit -am "feat: add @hash virtual field to query engine"
```

---

### Task 3: Default Index Config Bootstrap

**Files:**
- Modify: `aeordb-lib/src/server/mod.rs` (or where engine initialization happens)
- Modify: `aeordb-cli/src/commands/start.rs`

- [ ] **Step 1: Write default index config on first boot**

In `start.rs`, after the engine is created and bootstrap is done (around line 113), add:

```rust
// Write default global index config if it doesn't exist
{
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    let config_path = "/.config/indexes.json";

    // Check if default config already exists
    match ops.read_file(config_path) {
        Ok(_) => {
            // Config exists — don't overwrite user customizations
        }
        Err(_) => {
            let default_config = serde_json::json!({
                "glob": "**/*",
                "indexes": [
                    {"name": "@filename", "type": ["string", "trigram", "phonetic", "dmetaphone"]},
                    {"name": "@hash", "type": "trigram"},
                    {"name": "@created_at", "type": "timestamp"},
                    {"name": "@updated_at", "type": "timestamp"},
                    {"name": "@size", "type": "u64"},
                    {"name": "@content_type", "type": "string"}
                ]
            });
            let config_bytes = serde_json::to_vec_pretty(&default_config).unwrap();
            if let Err(e) = ops.store_file(&ctx, config_path, &config_bytes, Some("application/json")) {
                tracing::warn!("Failed to write default index config: {}", e);
            } else {
                tracing::info!("Created default index config at {}", config_path);
                // The store triggers auto-reindex via the existing .config/indexes.json watcher
            }
        }
    }
}
```

The existing auto-reindex trigger in `engine_routes.rs` detects `.config/indexes.json` creation and enqueues a reindex task. But since this happens in `start.rs` (not via the HTTP route), we need to manually enqueue:

```rust
if let Some(ref queue) = task_queue {
    let _ = queue.enqueue("reindex", serde_json::json!({"path": "/"}));
    tracing::info!("Enqueued initial global reindex");
}
```

- [ ] **Step 2: Ensure reindex skips internal paths**

In `task_worker.rs`, in the `execute_reindex` function, add an `is_internal_path` filter to the file listing. Find where `file_paths` are collected (the glob mode branch). Add:

```rust
.filter(|entry| !crate::engine::directory_ops::is_internal_path(&entry.path))
```

This must come BEFORE the glob filter.

- [ ] **Step 3: Build and test**

```bash
cargo build --release 2>&1 | tail -5
```

- [ ] **Step 4: Commit**

```bash
git commit -am "feat: default global index config written on first boot, triggers reindex"
```

---

### Task 4: Index Discovery Function

**Files:**
- Modify: `aeordb-lib/src/engine/index_store.rs`
- Test: `aeordb-lib/spec/engine/index_store_spec.rs`

- [ ] **Step 1: Add `discover_indexed_directories` to IndexManager**

In `index_store.rs`, add to `IndexManager`:

```rust
/// Discover all directories that contain .indexes/ subdirectories.
/// Returns a list of directory paths that have at least one index.
pub fn discover_indexed_directories(
    &self,
    engine: &StorageEngine,
    base_path: &str,
) -> EngineResult<Vec<String>> {
    let all_entries = crate::engine::directory_listing::list_directory_recursive(
        engine, base_path, -1, Some(".indexes"), None,
    )?;

    // Extract parent directories from .indexes paths
    let mut dirs: Vec<String> = all_entries
        .iter()
        .filter(|e| e.path.contains("/.indexes/"))
        .filter_map(|e| {
            let idx = e.path.find("/.indexes/")?;
            Some(e.path[..idx + 1].to_string())
        })
        .collect();

    dirs.sort();
    dirs.dedup();

    // Also check the base_path itself
    if self.list_indexes(base_path)?.len() > 0 && !dirs.contains(&base_path.to_string()) {
        dirs.insert(0, base_path.to_string());
    }

    Ok(dirs)
}
```

Note: This approach uses the existing `list_directory_recursive`. An alternative is to scan KV entries by type, which may be faster for large databases. Start with this approach and optimize later.

- [ ] **Step 2: Build and test**

```bash
cargo build --release 2>&1 | tail -5
```

- [ ] **Step 3: Commit**

```bash
git commit -am "feat: discover_indexed_directories for global search fan-out"
```

---

### Task 5: Global Search Engine Module

**Files:**
- Create: `aeordb-lib/src/engine/search.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`

- [ ] **Step 1: Create search.rs with broad and structured search**

Create `aeordb-lib/src/engine/search.rs`:

```rust
use crate::engine::errors::EngineResult;
use crate::engine::index_store::IndexManager;
use crate::engine::query_engine::QueryEngine;
use crate::engine::storage_engine::StorageEngine;

pub struct SearchResult {
    pub path: String,
    pub score: f64,
    pub matched_by: Vec<String>,
    pub source_dir: String,
    pub size: u64,
    pub content_type: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct SearchResults {
    pub results: Vec<SearchResult>,
    pub has_more: bool,
    pub total_count: Option<usize>,
}

/// Execute a global search across all indexed directories.
pub fn global_search(
    engine: &StorageEngine,
    query: Option<&str>,
    where_clause: Option<&serde_json::Value>,
    base_path: &str,
    limit: usize,
    offset: usize,
) -> EngineResult<SearchResults> {
    let index_manager = IndexManager::new(engine);
    let indexed_dirs = index_manager.discover_indexed_directories(engine, base_path)?;

    let mut all_results: Vec<SearchResult> = Vec::new();

    for dir in &indexed_dirs {
        // Broad search: query all fuzzy-capable fields
        if let Some(term) = query {
            let field_names = index_manager.list_indexes(dir)?;
            for field_name in &field_names {
                // Only search trigram, phonetic, soundex, dmetaphone indexes
                if is_fuzzy_capable(field_name) {
                    let base_field = field_name.split('.').next().unwrap_or(field_name);
                    if let Ok(results) = search_field_fuzzy(engine, dir, base_field, term) {
                        for (path, score) in results {
                            // Merge: if path already in results, keep higher score
                            if let Some(existing) = all_results.iter_mut().find(|r| r.path == path) {
                                if score > existing.score {
                                    existing.score = score;
                                }
                                if !existing.matched_by.contains(&base_field.to_string()) {
                                    existing.matched_by.push(base_field.to_string());
                                }
                            } else {
                                // Load file metadata
                                if let Ok(meta) = load_file_metadata(engine, &path) {
                                    all_results.push(SearchResult {
                                        path,
                                        score,
                                        matched_by: vec![base_field.to_string()],
                                        source_dir: dir.clone(),
                                        size: meta.0,
                                        content_type: meta.1,
                                        created_at: meta.2,
                                        updated_at: meta.3,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        // Structured search: delegate to existing query engine
        if let Some(clause) = where_clause {
            let query_engine = QueryEngine::new(engine);
            // Run the where clause against this directory
            // (reuse existing query_engine execute logic with dir as path)
            // Merge results into all_results
        }
    }

    // Sort by score descending
    all_results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    // Paginate
    let total = all_results.len();
    let has_more = offset + limit < total;
    let paginated: Vec<SearchResult> = all_results.into_iter().skip(offset).take(limit).collect();

    Ok(SearchResults {
        results: paginated,
        has_more,
        total_count: Some(total),
    })
}

fn is_fuzzy_capable(index_name: &str) -> bool {
    index_name.ends_with(".trigram")
        || index_name.ends_with(".phonetic")
        || index_name.ends_with(".soundex")
        || index_name.ends_with(".dmetaphone")
}

fn search_field_fuzzy(
    engine: &StorageEngine,
    dir: &str,
    field_name: &str,
    term: &str,
) -> EngineResult<Vec<(String, f64)>> {
    // Load the trigram index for this field and search
    let index_manager = IndexManager::new(engine);
    let hash_length = engine.hash_algo().hash_length();

    // Try trigram first (most common fuzzy index)
    if let Ok(Some(index)) = index_manager.load_index_by_strategy(dir, field_name, "trigram") {
        let term_bytes = term.as_bytes();
        let matches = index.search_similar(term_bytes, 0.3)?;
        let mut results = Vec::new();
        for entry in matches {
            // Resolve file_hash to path via FileRecord
            if let Ok(Some((_header, _key, value))) = engine.get_entry(&entry.file_hash) {
                if let Ok(record) = crate::engine::file_record::FileRecord::deserialize(
                    &value, hash_length, 0,
                ) {
                    results.push((record.path, entry.scalar));
                }
            }
        }
        return Ok(results);
    }

    Ok(Vec::new())
}

fn load_file_metadata(
    engine: &StorageEngine,
    path: &str,
) -> EngineResult<(u64, Option<String>, i64, i64)> {
    let algo = engine.hash_algo();
    let hash_length = algo.hash_length();
    let file_key = crate::engine::directory_ops::file_path_hash(path, &algo)?;
    match engine.get_entry(&file_key)? {
        Some((header, _key, value)) => {
            let record = crate::engine::file_record::FileRecord::deserialize(
                &value, hash_length, header.entry_version,
            )?;
            Ok((record.total_size, record.content_type, record.created_at, record.updated_at))
        }
        None => Err(crate::engine::errors::EngineError::NotFound(path.to_string())),
    }
}
```

Note: `search_similar` may not exist on `FieldIndex`. The actual search method depends on the index implementation. The agent implementing this must read `index_store.rs` to find the correct search/lookup methods and adapt accordingly.

- [ ] **Step 2: Register module**

In `aeordb-lib/src/engine/mod.rs`, add:

```rust
pub mod search;
```

- [ ] **Step 3: Build**

```bash
cargo build --release 2>&1 | tail -5
```

- [ ] **Step 4: Commit**

```bash
git commit -am "feat: global search engine module with fan-out across indexed directories"
```

---

### Task 6: POST /files/search Endpoint

**Files:**
- Modify: `aeordb-lib/src/server/engine_routes.rs`
- Modify: `aeordb-lib/src/server/mod.rs`

- [ ] **Step 1: Add search request struct and handler**

In `engine_routes.rs`, add:

```rust
#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: Option<String>,
    pub r#where: Option<serde_json::Value>,
    pub path: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub order_by: Option<Vec<serde_json::Value>>,
    pub include_total: Option<bool>,
    pub select: Option<Vec<String>>,
}

pub async fn global_search(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Json(payload): Json<SearchRequest>,
) -> Response {
    if payload.query.is_none() && payload.r#where.is_none() {
        return ErrorResponse::new("At least one of 'query' or 'where' is required")
            .with_status(StatusCode::BAD_REQUEST)
            .into_response();
    }

    let base_path = payload.path.as_deref().unwrap_or("/");
    let limit = payload.limit.unwrap_or(50).min(1000);
    let offset = payload.offset.unwrap_or(0);

    match crate::engine::search::global_search(
        &state.engine,
        payload.query.as_deref(),
        payload.r#where.as_ref(),
        base_path,
        limit,
        offset,
    ) {
        Ok(results) => {
            let items: Vec<serde_json::Value> = results.results.iter().map(|r| {
                serde_json::json!({
                    "path": r.path,
                    "score": r.score,
                    "matched_by": r.matched_by,
                    "source": r.source_dir,
                    "size": r.size,
                    "content_type": r.content_type,
                    "created_at": r.created_at,
                    "updated_at": r.updated_at,
                })
            }).collect();

            let mut response = serde_json::json!({
                "results": items,
                "has_more": results.has_more,
            });
            if let Some(total) = results.total_count {
                response["total_count"] = serde_json::json!(total);
            }
            (StatusCode::OK, Json(response)).into_response()
        }
        Err(error) => {
            ErrorResponse::new(format!("Search failed: {}", error))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}
```

- [ ] **Step 2: Register the route**

In `server/mod.rs`, add before `/files/query` (around line 247):

```rust
.route("/files/search", post(engine_routes::global_search))
```

- [ ] **Step 3: Build**

```bash
cargo build --release 2>&1 | tail -5
```

- [ ] **Step 4: Commit**

```bash
git commit -am "feat: POST /files/search endpoint for global cross-directory search"
```

---

### Task 7: Documentation

**Files:**
- Modify: `docs/src/api/querying.md`
- Modify: `docs/src/concepts/indexing.md`

- [ ] **Step 1: Document /files/search in querying.md**

Add a new section "Global Search" with the endpoint, request/response format, and examples for broad search, structured search, and combined mode.

- [ ] **Step 2: Document default indexes in indexing.md**

Add a section "Default Indexes" explaining:
- Built-in indexes on @filename, @hash, @created_at, @updated_at, @size, @content_type
- Auto-created on first boot
- How @-fields are extracted from FileRecord metadata
- How to customize (edit /.config/indexes.json)

- [ ] **Step 3: Build docs**

```bash
cd docs && mdbook build 2>&1 | tail -5; cd ..
```

- [ ] **Step 4: Commit**

```bash
git commit -am "docs: global search endpoint + default indexes"
```

---

### Task 8: Integration Test

**Files:**
- Test: `aeordb-lib/spec/engine/search_spec.rs` (new)

- [ ] **Step 1: Write end-to-end test**

```rust
#[test]
fn test_global_search_broad() {
    // Create engine, store files at /users/ and /docs/
    // Write index configs with trigram fields
    // Run indexing pipeline on each file
    // Call global_search with query="alice"
    // Verify results from both directories
}

#[test]
fn test_global_search_at_filename() {
    // Create engine, write default index config at /
    // Store files: /data/hello.json, /data/world.json
    // Run indexing pipeline
    // Search for "@filename contains hello"
    // Verify hello.json is found
}

#[test]
fn test_internal_paths_not_indexed() {
    // Verify .config/indexes.json itself is not in the index
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test --release -p aeordb --test search_spec 2>&1 | tail -10
```

- [ ] **Step 3: Commit**

```bash
git commit -am "test: global search integration tests"
```
