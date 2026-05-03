# Glob Indexing + Plural Source Resolver Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable index configs to index files across subdirectories via glob patterns, and support extracting multiple values from JSON arrays/objects via empty-string and regex source path segments.

**Architecture:** Two independent changes: (1) `PathIndexConfig` gains a `glob` field; the indexing pipeline walks ancestor directories, matches files against the glob, and stores indexes at the config owner's directory. (2) `resolve_source` becomes `resolve_sources` (plural), returning `Vec<Vec<u8>>` with fan-out support for empty-string and regex segments.

**Tech Stack:** Rust, regex crate, glob pattern matching (simple substring/wildcard — no external crate needed for the path matching)

---

### Task 1: Plural Source Resolver

**Files:**
- Modify: `aeordb-lib/src/engine/source_resolver.rs`
- Modify: `aeordb-lib/src/engine/mod.rs` (re-export)
- Test: `aeordb-lib/spec/engine/source_resolver_spec.rs`

- [ ] **Step 1: Write failing tests for the new plural resolver**

In `aeordb-lib/spec/engine/source_resolver_spec.rs`, add these tests:

```rust
use aeordb::engine::source_resolver::resolve_sources;

#[test]
fn resolve_sources_simple_key_returns_vec_of_one() {
    let json: serde_json::Value = serde_json::json!({"name": "Alice"});
    let source = vec![serde_json::Value::String("name".into())];
    let results = resolve_sources(&json, &source);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0], b"Alice");
}

#[test]
fn resolve_sources_empty_string_fans_out_array() {
    let json: serde_json::Value = serde_json::json!({
        "comments": [{"text": "hello"}, {"text": "world"}]
    });
    let source = vec![
        serde_json::Value::String("comments".into()),
        serde_json::Value::String("".into()),
        serde_json::Value::String("text".into()),
    ];
    let results = resolve_sources(&json, &source);
    assert_eq!(results.len(), 2);
    assert!(results.contains(&b"hello".to_vec()));
    assert!(results.contains(&b"world".to_vec()));
}

#[test]
fn resolve_sources_empty_string_fans_out_object_values() {
    let json: serde_json::Value = serde_json::json!({
        "tag_color": "red", "tag_size": "large"
    });
    let source = vec![serde_json::Value::String("".into())];
    let results = resolve_sources(&json, &source);
    assert_eq!(results.len(), 2);
    assert!(results.contains(&b"red".to_vec()));
    assert!(results.contains(&b"large".to_vec()));
}

#[test]
fn resolve_sources_regex_filters_object_keys() {
    let json: serde_json::Value = serde_json::json!({
        "tag_color": "red", "tag_size": "large", "name": "foo"
    });
    let source = vec![serde_json::Value::String("/^tag_/".into())];
    let results = resolve_sources(&json, &source);
    assert_eq!(results.len(), 2);
    assert!(results.contains(&b"red".to_vec()));
    assert!(results.contains(&b"large".to_vec()));
}

#[test]
fn resolve_sources_regex_case_insensitive() {
    let json: serde_json::Value = serde_json::json!({
        "Name": "Alice", "AGE": "30", "email": "a@b.com"
    });
    let source = vec![serde_json::Value::String("/^name$/i".into())];
    let results = resolve_sources(&json, &source);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0], b"Alice");
}

#[test]
fn resolve_sources_empty_string_on_nested_arrays() {
    let json: serde_json::Value = serde_json::json!({
        "sessions": [
            {"tags": ["a", "b"]},
            {"tags": ["c"]}
        ]
    });
    let source = vec![
        serde_json::Value::String("sessions".into()),
        serde_json::Value::String("".into()),
        serde_json::Value::String("tags".into()),
        serde_json::Value::String("".into()),
    ];
    let results = resolve_sources(&json, &source);
    assert_eq!(results.len(), 3);
    assert!(results.contains(&b"a".to_vec()));
    assert!(results.contains(&b"b".to_vec()));
    assert!(results.contains(&b"c".to_vec()));
}

#[test]
fn resolve_sources_no_match_returns_empty() {
    let json: serde_json::Value = serde_json::json!({"name": "Alice"});
    let source = vec![serde_json::Value::String("missing".into())];
    let results = resolve_sources(&json, &source);
    assert!(results.is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test --release -p aeordb --test source_resolver_spec 2>&1 | tail -10
```

Expected: compilation error (`resolve_sources` not found)

- [ ] **Step 3: Implement `resolve_sources` and `walk_paths`**

Replace the contents of `aeordb-lib/src/engine/source_resolver.rs`:

```rust
use regex::Regex;

/// Check if a string is a regex segment: starts with `/`, has another `/`.
fn parse_regex_segment(s: &str) -> Option<Regex> {
    if !s.starts_with('/') || s.len() < 2 { return None; }
    let last_slash = s.rfind('/').filter(|&i| i > 0)?;
    if last_slash == 0 { return None; }
    let pattern = &s[1..last_slash];
    let flags = &s[last_slash + 1..];
    let full_pattern = if flags.contains('i') {
        format!("(?i){}", pattern)
    } else {
        pattern.to_string()
    };
    Regex::new(&full_pattern).ok()
}

/// Resolve a source path against JSON, returning multiple values (fan-out).
///
/// Segments:
///   - Non-empty string (no `/` delimiters) → object key lookup
///   - Integer → array index or object string-key
///   - Empty string `""` → fan out: all array elements or all object values
///   - Regex `/pattern/flags` → fan out: filter array elements or object keys
pub fn resolve_sources(json: &serde_json::Value, source: &[serde_json::Value]) -> Vec<Vec<u8>> {
    let resolved = walk_paths(json, source);
    resolved
        .into_iter()
        .map(|v| crate::engine::json_parser::json_value_to_bytes(&v))
        .collect()
}

/// Walk a JSON value following path segments, with fan-out for empty strings and regex.
/// Returns all resolved values.
pub fn walk_paths(json: &serde_json::Value, segments: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut current_set = vec![json.clone()];

    for segment in segments {
        let mut next_set = Vec::new();

        for current in &current_set {
            match segment {
                serde_json::Value::String(key) if key.is_empty() => {
                    // Empty string: fan out to all elements/values
                    if let Some(arr) = current.as_array() {
                        next_set.extend(arr.iter().cloned());
                    } else if let Some(obj) = current.as_object() {
                        next_set.extend(obj.values().cloned());
                    }
                }
                serde_json::Value::String(key) if key.starts_with('/') => {
                    // Regex segment
                    if let Some(re) = parse_regex_segment(key) {
                        if let Some(obj) = current.as_object() {
                            // Match keys
                            for (k, v) in obj {
                                if re.is_match(k) {
                                    next_set.push(v.clone());
                                }
                            }
                        } else if let Some(arr) = current.as_array() {
                            // Match stringified elements
                            for elem in arr {
                                let s = match elem {
                                    serde_json::Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                };
                                if re.is_match(&s) {
                                    next_set.push(elem.clone());
                                }
                            }
                        }
                    }
                }
                serde_json::Value::String(key) => {
                    // Regular key lookup
                    if let Some(val) = current.get(key.as_str()) {
                        next_set.push(val.clone());
                    }
                }
                serde_json::Value::Number(n) => {
                    if let Some(idx) = n.as_u64() {
                        let idx = idx as usize;
                        if current.is_array() {
                            if let Some(val) = current.get(idx) {
                                next_set.push(val.clone());
                            }
                        } else if let Some(val) = current.get(&idx.to_string()) {
                            next_set.push(val.clone());
                        }
                    }
                }
                _ => {} // bool, null, object, array — skip
            }
        }

        current_set = next_set;
        if current_set.is_empty() {
            return Vec::new();
        }
    }

    current_set
}

// Backward-compatible single-value resolver (delegates to resolve_sources)
pub fn resolve_source(json: &serde_json::Value, source: &[serde_json::Value]) -> Option<Vec<u8>> {
    resolve_sources(json, source).into_iter().next()
}

// Backward-compatible single-value walker
pub fn walk_path(json: &serde_json::Value, segments: &[serde_json::Value]) -> Option<serde_json::Value> {
    walk_paths(json, segments).into_iter().next()
}
```

- [ ] **Step 4: Update mod.rs re-export**

In `aeordb-lib/src/engine/mod.rs`, find the re-export line (around line 130):
```rust
pub use source_resolver::{resolve_source, walk_path};
```
Change to:
```rust
pub use source_resolver::{resolve_source, resolve_sources, walk_path, walk_paths};
```

- [ ] **Step 5: Add regex dependency if not present**

Check `aeordb-lib/Cargo.toml` for `regex`. If missing, add:
```toml
regex = "1"
```

- [ ] **Step 6: Run tests**

```bash
cargo test --release -p aeordb --test source_resolver_spec 2>&1 | tail -15
```

Expected: all tests pass

- [ ] **Step 7: Commit**

```bash
git add aeordb-lib/src/engine/source_resolver.rs aeordb-lib/src/engine/mod.rs aeordb-lib/spec/engine/source_resolver_spec.rs
git commit -m "feat: plural source resolver with empty-string and regex fan-out"
```

---

### Task 2: Indexing Pipeline Uses Plural Sources

**Files:**
- Modify: `aeordb-lib/src/engine/indexing_pipeline.rs`
- Test: `aeordb-lib/spec/engine/pipeline_spec.rs`

- [ ] **Step 1: Update `index_field` to use `resolve_sources` and insert multiple entries**

In `indexing_pipeline.rs`, find the `index_field` method (around line 155). Replace the source resolution section that calls `resolve_source` with `resolve_sources`, and loop over the results:

Find the block (approximately lines 160-201) that:
1. Calls `resolve_source(json_data, segments)` or similar
2. Gets a single `field_value`
3. Calls `index.remove(file_key)` then `index.insert_expanded(&field_value, ...)`

Replace with logic that:
1. Calls `resolve_sources(json_data, segments)` to get `Vec<Vec<u8>>`
2. Calls `index.remove(file_key)` once (clears all old entries for this file)
3. Loops: for each value in the vec, calls `index.insert_expanded(&value, file_key.to_vec())`

Update the import from `use crate::engine::source_resolver::resolve_source` to `use crate::engine::source_resolver::resolve_sources`.

- [ ] **Step 2: Write a test for multi-value indexing**

In `aeordb-lib/spec/engine/pipeline_spec.rs`, add a test that stores a JSON file with an array, indexes with `source: ["items", "", "name"]`, and verifies multiple index entries are created. Use the existing `store_index_config` and `make_simple_config` helpers as patterns.

- [ ] **Step 3: Run tests**

```bash
cargo test --release -p aeordb --test pipeline_spec 2>&1 | tail -10
```

- [ ] **Step 4: Commit**

```bash
git commit -am "feat: indexing pipeline inserts multiple entries per field via resolve_sources"
```

---

### Task 3: Add `glob` Field to PathIndexConfig

**Files:**
- Modify: `aeordb-lib/src/engine/index_config.rs`
- Test: `aeordb-lib/spec/engine/index_config_spec.rs` (or inline tests)

- [ ] **Step 1: Add `glob` field to `PathIndexConfig`**

In `index_config.rs`, add `pub glob: Option<String>` to the `PathIndexConfig` struct (after `logging`, around line 24):

```rust
#[derive(Debug, Clone)]
pub struct PathIndexConfig {
  pub indexes: Vec<IndexFieldConfig>,
  pub parser: Option<String>,
  pub parser_memory_limit: Option<String>,
  pub logging: bool,
  pub glob: Option<String>,
}
```

- [ ] **Step 2: Update `serialize` to include glob**

In the `serialize` method, add after the logging field:
```rust
if let Some(ref glob) = self.glob {
    map.insert("glob".to_string(), serde_json::Value::String(glob.clone()));
}
```

- [ ] **Step 3: Update `deserialize` to parse glob**

In the `deserialize` method, add alongside the other field extractions (around lines 79-89):
```rust
let glob = root.get("glob").and_then(|v| v.as_str()).map(|s| s.to_string());
```

And include it in the struct construction:
```rust
Ok(PathIndexConfig {
    indexes,
    parser,
    parser_memory_limit,
    logging,
    glob,
})
```

- [ ] **Step 4: Build and verify**

```bash
cargo build --release 2>&1 | tail -5
```

- [ ] **Step 5: Commit**

```bash
git commit -am "feat: add glob field to PathIndexConfig"
```

---

### Task 4: Ancestor Config Discovery in Indexing Pipeline

**Files:**
- Modify: `aeordb-lib/src/engine/indexing_pipeline.rs`
- Test: `aeordb-lib/spec/engine/pipeline_spec.rs`

- [ ] **Step 1: Add `find_config_for_path` method to IndexingPipeline**

This method walks up the ancestor chain looking for a config with a matching glob. Add to `IndexingPipeline`:

```rust
/// Find the index config that applies to a file path.
/// Checks the immediate parent first (existing behavior), then walks
/// up ancestor directories looking for configs with a matching `glob`.
/// Returns (config, config_directory) or None.
fn find_config_for_path(&self, file_path: &str) -> EngineResult<Option<(PathIndexConfig, String)>> {
    let normalized = normalize_path(file_path);
    let parent = parent_path(&normalized).unwrap_or_else(|| "/".to_string());

    // 1. Check immediate parent (existing behavior — handles non-glob configs)
    if let Some(config) = self.load_config(&parent)? {
        if config.glob.is_none() {
            return Ok(Some((config, parent)));
        }
        // Parent has a glob config — check if THIS file matches its glob
        let relative = normalized.trim_start_matches(parent.trim_end_matches('/'));
        let relative = relative.trim_start_matches('/');
        if glob_matches(&config.glob.as_ref().unwrap(), relative) {
            return Ok(Some((config, parent)));
        }
    }

    // 2. Walk up ancestors looking for glob configs
    let mut ancestor = parent_path(&parent);
    while let Some(ref anc) = ancestor {
        if let Some(config) = self.load_config(anc)? {
            if let Some(ref glob_pattern) = config.glob {
                let prefix = anc.trim_end_matches('/');
                let relative = normalized.trim_start_matches(prefix).trim_start_matches('/');
                if glob_matches(glob_pattern, relative) {
                    return Ok(Some((config, anc.clone())));
                }
            }
        }
        if anc == "/" { break; }
        ancestor = parent_path(anc);
    }

    Ok(None)
}
```

- [ ] **Step 2: Add `glob_matches` helper function**

Add a simple glob matcher at the module level in `indexing_pipeline.rs`:

```rust
/// Simple glob pattern match for file paths.
/// Supports: `*` (matches one path segment), `**` (matches any depth),
/// `?` (single char). Paths are compared segment-by-segment.
fn glob_matches(pattern: &str, path: &str) -> bool {
    let pat_segments: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let path_segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    glob_match_segments(&pat_segments, &path_segments)
}

fn glob_match_segments(pattern: &[&str], path: &[&str]) -> bool {
    if pattern.is_empty() {
        return path.is_empty();
    }
    if pattern[0] == "**" {
        // ** matches zero or more segments
        for i in 0..=path.len() {
            if glob_match_segments(&pattern[1..], &path[i..]) {
                return true;
            }
        }
        return false;
    }
    if path.is_empty() {
        return false;
    }
    if segment_matches(pattern[0], path[0]) {
        glob_match_segments(&pattern[1..], &path[1..])
    } else {
        false
    }
}

fn segment_matches(pattern: &str, segment: &str) -> bool {
    if pattern == "*" { return true; }
    let mut pi = pattern.chars().peekable();
    let mut si = segment.chars().peekable();
    while let Some(pc) = pi.next() {
        match pc {
            '?' => { if si.next().is_none() { return false; } }
            '*' => {
                // * within a segment matches any chars
                let rest_pattern: String = pi.collect();
                for i in 0..=segment.len() - si.clone().count() {
                    let rest_segment: String = si.clone().skip(i).collect();
                    if segment_matches(&rest_pattern, &rest_segment) { return true; }
                }
                return false;
            }
            c => {
                if si.next() != Some(c) { return false; }
            }
        }
    }
    si.next().is_none()
}
```

- [ ] **Step 3: Update `run` method to use `find_config_for_path`**

In the `run` method (or `index_file`/main entry point, around line 36), replace:
```rust
let parent = parent_path(&normalized).unwrap_or_else(|| "/".to_string());
let config = match self.load_config(&parent)? {
    Some(c) => c,
    None => return Ok(()),
};
```

With:
```rust
let (config, config_dir) = match self.find_config_for_path(path)? {
    Some(pair) => pair,
    None => return Ok(()),
};
```

Then use `config_dir` instead of `parent` everywhere indexes are stored/loaded in the method body (calls to `index_manager.load_index`, `index_manager.save_index`, `index_manager.create_index`).

- [ ] **Step 4: Write tests for glob matching and ancestor discovery**

In `pipeline_spec.rs`, add tests:
- Store a config at `/sessions/` with `glob: "*/session.json"`, store a file at `/sessions/s1/session.json`, verify it gets indexed.
- Store a file at `/sessions/s1/notes.txt`, verify it is NOT indexed.
- Test `glob_matches("*/session.json", "s1/session.json")` → true
- Test `glob_matches("*/session.json", "s1/notes.txt")` → false
- Test `glob_matches("**/*.json", "a/b/c/file.json")` → true

- [ ] **Step 5: Run tests**

```bash
cargo test --release -p aeordb --test pipeline_spec 2>&1 | tail -15
```

- [ ] **Step 6: Commit**

```bash
git commit -am "feat: ancestor config discovery with glob matching for subdirectory indexing"
```

---

### Task 5: Reindex Task Supports Glob

**Files:**
- Modify: `aeordb-lib/src/engine/task_worker.rs`

- [ ] **Step 1: Update `execute_reindex` to use recursive listing when glob is present**

In `task_worker.rs`, find the `execute_reindex` function (around line 179). After loading the config (which already happens), check for the glob field.

Replace the directory listing section (around lines 203-210):

```rust
let entries = ops.list_directory(path)
    .map_err(|e| format!("cannot list directory {}: {}", path, e))?;
let mut file_entries: Vec<_> = entries
    .into_iter()
    .filter(|entry| entry.entry_type == EntryType::FileRecord.to_u8())
    .collect();
```

With:

```rust
let file_entries = if let Some(ref glob_pattern) = config.glob {
    // Glob mode: recursive listing, filter by glob
    let all_entries = crate::engine::directory_listing::list_directory_recursive(
        engine, path, -1, None, None,
    ).map_err(|e| format!("cannot list directory {}: {}", path, e))?;

    let prefix = path.trim_end_matches('/');
    let mut filtered: Vec<_> = all_entries
        .into_iter()
        .filter(|entry| {
            if entry.entry_type != "file" { return false; }
            let relative = entry.path.trim_start_matches(prefix).trim_start_matches('/');
            crate::engine::indexing_pipeline::glob_matches(glob_pattern, relative)
        })
        .collect();
    filtered.sort_by(|a, b| a.path.cmp(&b.path));
    filtered
} else {
    // Non-glob: direct children only (existing behavior)
    let entries = ops.list_directory(path)
        .map_err(|e| format!("cannot list directory {}: {}", path, e))?;
    let mut file_entries: Vec<_> = entries
        .into_iter()
        .filter(|entry| entry.entry_type == EntryType::FileRecord.to_u8())
        .collect();
    file_entries.sort_by(|a, b| a.name.cmp(&b.name));
    file_entries
};
```

Note: The types differ between `list_directory` (returns `ChildEntry`) and `list_directory_recursive` (returns `ListingEntry`). You'll need to adapt the downstream code that reads `entry.name` to handle both cases, or normalize them. The simplest approach: extract the file path from each entry type and use it consistently in the processing loop.

Make `glob_matches` public (`pub fn`) in `indexing_pipeline.rs` so `task_worker.rs` can import it.

- [ ] **Step 2: Build and verify**

```bash
cargo build --release 2>&1 | tail -10
```

Fix any type mismatches between `ChildEntry` and `ListingEntry` in the processing loop.

- [ ] **Step 3: Commit**

```bash
git commit -am "feat: reindex task uses recursive listing with glob filter"
```

---

### Task 6: Deletion Cleanup for Glob-Indexed Files

**Files:**
- Modify: `aeordb-lib/src/engine/directory_ops.rs`

- [ ] **Step 1: Update `delete_file_with_indexing` to check ancestor configs**

In `directory_ops.rs`, find `delete_file_with_indexing` (around line 1534). Currently it only removes index entries from the immediate parent's `.indexes/`. Add ancestor traversal:

After the existing parent index cleanup block, add:

```rust
// Also check ancestor directories for glob-based configs
let pipeline = crate::engine::indexing_pipeline::IndexingPipeline::new(self.engine);
if let Ok(Some((_config, config_dir))) = pipeline.find_config_for_path(&normalized) {
    if config_dir != parent {
        // File was indexed by an ancestor config — clean up those indexes too
        let ancestor_index_names = index_manager.list_indexes(&config_dir)?;
        for field_name in &ancestor_index_names {
            if let Some(mut index) = index_manager.load_index(&config_dir, field_name)? {
                index.remove(&file_key);
                index_manager.save_index(&config_dir, &index)?;
            }
        }
    }
}
```

Note: `find_config_for_path` needs to be `pub` on `IndexingPipeline` for this to work. Update its visibility.

- [ ] **Step 2: Build and verify**

```bash
cargo build --release 2>&1 | tail -5
```

- [ ] **Step 3: Commit**

```bash
git commit -am "feat: deletion cleanup checks ancestor glob configs for index removal"
```

---

### Task 7: Integration Test — End-to-End Glob Indexing

**Files:**
- Test: `aeordb-lib/spec/engine/pipeline_spec.rs`

- [ ] **Step 1: Write an end-to-end test**

```rust
#[test]
fn test_glob_indexing_subdirectory_files() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    // Store config at /sessions/ with glob
    let config_json = r#"{
        "glob": "*/session.json",
        "indexes": [
            {"name": "patient", "type": "string"}
        ]
    }"#;
    ops.store_file(&ctx, "/sessions/.config/indexes.json", config_json.as_bytes(), Some("application/json")).unwrap();

    // Store files in subdirectories
    let session1 = r#"{"patient": "Alice"}"#;
    let session2 = r#"{"patient": "Bob"}"#;
    ops.store_file(&ctx, "/sessions/s1/session.json", session1.as_bytes(), Some("application/json")).unwrap();
    ops.store_file(&ctx, "/sessions/s2/session.json", session2.as_bytes(), Some("application/json")).unwrap();

    // Run indexing pipeline on each
    let pipeline = IndexingPipeline::new(&engine);
    pipeline.run(&ctx, "/sessions/s1/session.json", session1.as_bytes(), Some("application/json")).unwrap();
    pipeline.run(&ctx, "/sessions/s2/session.json", session2.as_bytes(), Some("application/json")).unwrap();

    // Verify indexes are at /sessions/.indexes/ (not in subdirectories)
    let index_manager = IndexManager::new(&engine);
    let indexes = index_manager.list_indexes("/sessions/").unwrap();
    assert!(indexes.contains(&"patient".to_string()), "patient index should exist at /sessions/");

    // Load the index and verify both entries
    let index = index_manager.load_index("/sessions/", "patient").unwrap().unwrap();
    assert!(index.entries.len() >= 2, "should have entries for both files");
}
```

- [ ] **Step 2: Run test**

```bash
cargo test --release -p aeordb --test pipeline_spec test_glob_indexing 2>&1 | tail -10
```

- [ ] **Step 3: Write a test for plural sources with array fan-out end-to-end**

```rust
#[test]
fn test_plural_source_indexes_array_elements() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    // Config with empty-string source for array fan-out
    let config_json = r#"{
        "indexes": [
            {"name": "tag", "type": "string", "source": ["tags", ""]}
        ]
    }"#;
    ops.store_file(&ctx, "/docs/.config/indexes.json", config_json.as_bytes(), Some("application/json")).unwrap();

    let doc = r#"{"tags": ["rust", "database", "aeordb"]}"#;
    ops.store_file(&ctx, "/docs/readme.json", doc.as_bytes(), Some("application/json")).unwrap();

    let pipeline = IndexingPipeline::new(&engine);
    pipeline.run(&ctx, "/docs/readme.json", doc.as_bytes(), Some("application/json")).unwrap();

    let index_manager = IndexManager::new(&engine);
    let index = index_manager.load_index("/docs/", "tag").unwrap().unwrap();
    assert!(index.entries.len() >= 3, "should have 3 entries for 3 tags");
}
```

- [ ] **Step 4: Run all tests**

```bash
cargo test --release -p aeordb --test pipeline_spec 2>&1 | tail -15
cargo test --release -p aeordb --test source_resolver_spec 2>&1 | tail -10
```

- [ ] **Step 5: Commit**

```bash
git commit -am "test: end-to-end glob indexing and plural source fan-out"
```
