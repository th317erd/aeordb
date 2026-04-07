# Sorting + Pagination Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add ORDER BY sorting (single/multi-field, virtual `@` fields), offset pagination, cursor-based pagination with version-locked stability, and a default limit of 20 to all queries.

**Architecture:** Sort happens post-filter. The filter produces `HashSet<file_hash>`, then we load sort field values from indexes, sort in-memory, paginate, and wrap in an envelope with `has_more`/cursors. Cursors encode sort key + file_hash + version_hash for stability across pages. Virtual fields (`@score`, `@path`, etc.) are sorted from QueryResult metadata, not indexes.

**Tech Stack:** Rust, serde_json, base64

**Spec:** `bot-docs/plan/query-sorting-pagination.md`

---

### Task 1: Add SortField, SortDirection, and PaginatedResult types

**Files:**
- Modify: `aeordb-lib/src/engine/query_engine.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`

- [ ] **Step 1: Add types to query_engine.rs**

Add after the `QueryStrategy` enum (around line 96):

```rust
/// Sort direction for ORDER BY.
#[derive(Debug, Clone)]
pub enum SortDirection {
    Asc,
    Desc,
}

/// A single sort field in an ORDER BY clause.
#[derive(Debug, Clone)]
pub struct SortField {
    /// Field name, or virtual field prefixed with @ (e.g., "@score", "@path")
    pub field: String,
    /// Sort direction (default: Asc)
    pub direction: SortDirection,
}

/// Default limit applied when no explicit limit is provided.
pub const DEFAULT_QUERY_LIMIT: usize = 20;

/// Paginated query response wrapping results with metadata.
#[derive(Debug)]
pub struct PaginatedResult {
    pub results: Vec<QueryResult>,
    pub total_count: Option<u64>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub prev_cursor: Option<String>,
    pub default_limit_hit: bool,
}
```

- [ ] **Step 2: Update Query struct with new fields**

Change the `Query` struct:

```rust
pub struct Query {
    pub path: String,
    pub field_queries: Vec<FieldQuery>,
    pub node: Option<QueryNode>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub order_by: Vec<SortField>,
    pub after: Option<String>,
    pub before: Option<String>,
    pub include_total: bool,
    pub strategy: QueryStrategy,
}
```

- [ ] **Step 3: Update all Query construction sites**

Search for `Query {` in the codebase. Every place that constructs a Query needs the new fields with defaults:

```rust
offset: None,
order_by: Vec::new(),
after: None,
before: None,
include_total: false,
```

Key files: `engine_routes.rs` (query_endpoint), all test files that construct Query directly. Use `grep -rn "Query {" aeordb-lib/` to find them all.

- [ ] **Step 4: Export new types from mod.rs**

Add to the `pub use query_engine::{...}` line:

```rust
SortField, SortDirection, PaginatedResult, DEFAULT_QUERY_LIMIT,
```

- [ ] **Step 5: Build and verify**

Run: `cargo build 2>&1 | tail -20`
Then: `cargo test 2>&1 | grep "test result" | awk '{sum += $4; fail += $6} END {print "Passed:", sum, "Failed:", fail}'`

All 1,811 tests must pass.

- [ ] **Step 6: Commit**

```bash
git add aeordb-lib/src/engine/query_engine.rs aeordb-lib/src/engine/mod.rs
# + any files where Query construction was updated
git commit -m "Add SortField, SortDirection, PaginatedResult types + Query fields"
```

---

### Task 2: Default limit + PaginatedResult in execute()

**Files:**
- Modify: `aeordb-lib/src/engine/query_engine.rs`
- Create: `aeordb-lib/spec/engine/query_pagination_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Write tests for default limit**

Create `aeordb-lib/spec/engine/query_pagination_spec.rs`:

```rust
use std::sync::Arc;
use aeordb::engine::{
    StorageEngine, DirectoryOps, IndexManager, RequestContext,
    QueryEngine, Query, QueryNode, FieldQuery, QueryOp, QueryStrategy,
    PaginatedResult, SortField, SortDirection, DEFAULT_QUERY_LIMIT,
};
use aeordb::engine::index_config::{PathIndexConfig, IndexFieldConfig};
use aeordb::server::create_temp_engine_for_tests;

fn setup_test_data() -> (Arc<StorageEngine>, tempfile::TempDir) {
    let (engine, temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    // Store index config
    let config = PathIndexConfig {
        indexes: vec![
            IndexFieldConfig { name: "name".to_string(), index_type: "string".to_string(), source: None, min: None, max: None },
            IndexFieldConfig { name: "age".to_string(), index_type: "u64".to_string(), source: None, min: None, max: None },
        ],
        parser: None,
        parser_memory_limit: None,
        logging: false,
    };
    let config_data = config.serialize();
    ops.store_file(&ctx, "/people/.config/indexes.json", &config_data, Some("application/json")).unwrap();

    // Store 30 people (more than default limit of 20)
    for i in 0..30 {
        let json = format!(r#"{{"name":"person_{:02}","age":{}}}"#, i, 20 + i);
        ops.store_file(&ctx, &format!("/people/person_{:02}.json", i), json.as_bytes(), Some("application/json")).unwrap();
    }

    (engine, temp)
}

#[test]
fn test_default_limit_applied_when_no_limit() {
    let (engine, _temp) = setup_test_data();
    let qe = QueryEngine::new(&engine);

    let query = Query {
        path: "/people/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(20u64.to_be_bytes().to_vec()),
        })),
        limit: None, // no explicit limit
        offset: None,
        order_by: vec![],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let result = qe.execute_paginated(&query).unwrap();
    assert!(result.results.len() <= DEFAULT_QUERY_LIMIT);
    assert!(result.default_limit_hit);
    assert!(result.has_more);
}

#[test]
fn test_explicit_limit_overrides_default() {
    let (engine, _temp) = setup_test_data();
    let qe = QueryEngine::new(&engine);

    let query = Query {
        path: "/people/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(20u64.to_be_bytes().to_vec()),
        })),
        limit: Some(5),
        offset: None,
        order_by: vec![],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let result = qe.execute_paginated(&query).unwrap();
    assert_eq!(result.results.len(), 5);
    assert!(!result.default_limit_hit);
    assert!(result.has_more);
}

#[test]
fn test_offset_skips_results() {
    let (engine, _temp) = setup_test_data();
    let qe = QueryEngine::new(&engine);

    let query_page1 = Query {
        path: "/people/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(20u64.to_be_bytes().to_vec()),
        })),
        limit: Some(5),
        offset: None,
        order_by: vec![SortField { field: "age".to_string(), direction: SortDirection::Asc }],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let query_page2 = Query {
        path: "/people/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(20u64.to_be_bytes().to_vec()),
        })),
        limit: Some(5),
        offset: Some(5),
        order_by: vec![SortField { field: "age".to_string(), direction: SortDirection::Asc }],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let page1 = qe.execute_paginated(&query_page1).unwrap();
    let page2 = qe.execute_paginated(&query_page2).unwrap();

    // Pages should not overlap
    let page1_paths: Vec<&str> = page1.results.iter().map(|r| r.file_record.path.as_str()).collect();
    let page2_paths: Vec<&str> = page2.results.iter().map(|r| r.file_record.path.as_str()).collect();
    for path in &page2_paths {
        assert!(!page1_paths.contains(path), "page2 should not overlap with page1");
    }
}

#[test]
fn test_has_more_false_when_all_results_returned() {
    let (engine, _temp) = setup_test_data();
    let qe = QueryEngine::new(&engine);

    let query = Query {
        path: "/people/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(20u64.to_be_bytes().to_vec()),
        })),
        limit: Some(100), // more than 30 results
        offset: None,
        order_by: vec![],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let result = qe.execute_paginated(&query).unwrap();
    assert!(!result.has_more);
}

#[test]
fn test_include_total_returns_count() {
    let (engine, _temp) = setup_test_data();
    let qe = QueryEngine::new(&engine);

    let query = Query {
        path: "/people/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(20u64.to_be_bytes().to_vec()),
        })),
        limit: Some(5),
        offset: None,
        order_by: vec![],
        after: None,
        before: None,
        include_total: true,
        strategy: QueryStrategy::Full,
    };

    let result = qe.execute_paginated(&query).unwrap();
    assert!(result.total_count.is_some());
    assert!(result.total_count.unwrap() >= 5);
}

#[test]
fn test_empty_result_set() {
    let (engine, _temp) = setup_test_data();
    let qe = QueryEngine::new(&engine);

    let query = Query {
        path: "/people/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(999u64.to_be_bytes().to_vec()),
        })),
        limit: Some(20),
        offset: None,
        order_by: vec![],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let result = qe.execute_paginated(&query).unwrap();
    assert!(result.results.is_empty());
    assert!(!result.has_more);
    assert!(!result.default_limit_hit);
}
```

Register in Cargo.toml:
```toml
[[test]]
name = "query_pagination_spec"
path = "spec/engine/query_pagination_spec.rs"
```

- [ ] **Step 2: Implement execute_paginated**

Add a new method `execute_paginated` to `QueryEngine` that wraps `execute` with pagination:

```rust
pub fn execute_paginated(&self, query: &Query) -> EngineResult<PaginatedResult> {
    // Determine effective limit
    let explicit_limit = query.limit.is_some();
    let effective_limit = query.limit.unwrap_or(DEFAULT_QUERY_LIMIT);

    // Execute the full filter (existing execute logic, but without limit)
    let mut all_results = self.execute_unlimited(query)?;

    // Sort if order_by is specified
    if !query.order_by.is_empty() {
        self.sort_results(&mut all_results, &query.order_by, &query.path)?;
    }

    let total_count = if query.include_total {
        Some(all_results.len() as u64)
    } else {
        None
    };

    // Apply offset
    let offset = query.offset.unwrap_or(0);
    if offset > 0 && offset < all_results.len() {
        all_results = all_results.into_iter().skip(offset).collect();
    } else if offset >= all_results.len() {
        all_results.clear();
    }

    // Determine has_more before truncating
    let has_more = all_results.len() > effective_limit;

    // Apply limit
    all_results.truncate(effective_limit);

    let default_limit_hit = !explicit_limit && has_more;

    // Build cursors (Task 4 — leave as None for now)
    let next_cursor = None;
    let prev_cursor = None;

    Ok(PaginatedResult {
        results: all_results,
        total_count,
        has_more,
        next_cursor,
        prev_cursor,
        default_limit_hit,
    })
}

/// Execute a query without applying limit (for pagination to work on the full set).
fn execute_unlimited(&self, query: &Query) -> EngineResult<Vec<QueryResult>> {
    // Same as execute() but without the limit truncation at the end.
    // Refactor: extract the core logic from execute() into this method,
    // and have execute() call execute_unlimited then truncate.
    // ... (copy the execute logic, remove the limit truncation)
}
```

The cleanest approach: refactor `execute()` to call `execute_paginated()` and return just the results (for backward compat). Or have both call a shared internal method.

- [ ] **Step 3: Run tests**

Run: `cargo test --test query_pagination_spec 2>&1`
Expected: All pass

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/engine/query_engine.rs aeordb-lib/spec/engine/query_pagination_spec.rs aeordb-lib/Cargo.toml
git commit -m "Add execute_paginated with default limit, offset, has_more, include_total"
```

---

### Task 3: Single-field and multi-field sorting

**Files:**
- Modify: `aeordb-lib/src/engine/query_engine.rs`
- Modify: `aeordb-lib/spec/engine/query_pagination_spec.rs`

- [ ] **Step 1: Write sorting tests**

Add to `query_pagination_spec.rs`:

```rust
#[test]
fn test_sort_by_field_asc() {
    let (engine, _temp) = setup_test_data();
    let qe = QueryEngine::new(&engine);

    let query = Query {
        path: "/people/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(20u64.to_be_bytes().to_vec()),
        })),
        limit: Some(10),
        offset: None,
        order_by: vec![SortField { field: "age".to_string(), direction: SortDirection::Asc }],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let result = qe.execute_paginated(&query).unwrap();
    // Verify results are sorted by age ascending
    // We can't easily extract the age from QueryResult (it's in the file),
    // but we can verify the file paths are in order since person_00 has age 20, etc.
    let paths: Vec<&str> = result.results.iter().map(|r| r.file_record.path.as_str()).collect();
    for i in 1..paths.len() {
        assert!(paths[i-1] <= paths[i], "results should be sorted: {} <= {}", paths[i-1], paths[i]);
    }
}

#[test]
fn test_sort_by_field_desc() {
    let (engine, _temp) = setup_test_data();
    let qe = QueryEngine::new(&engine);

    let query = Query {
        path: "/people/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(20u64.to_be_bytes().to_vec()),
        })),
        limit: Some(10),
        offset: None,
        order_by: vec![SortField { field: "age".to_string(), direction: SortDirection::Desc }],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let result = qe.execute_paginated(&query).unwrap();
    let paths: Vec<&str> = result.results.iter().map(|r| r.file_record.path.as_str()).collect();
    for i in 1..paths.len() {
        assert!(paths[i-1] >= paths[i], "results should be reverse sorted: {} >= {}", paths[i-1], paths[i]);
    }
}

#[test]
fn test_sort_by_virtual_field_score() {
    let (engine, _temp) = setup_test_data();
    let qe = QueryEngine::new(&engine);

    // Use a fuzzy query that produces varied scores
    // Then sort by @score desc (should be default behavior anyway)
    let query = Query {
        path: "/people/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(20u64.to_be_bytes().to_vec()),
        })),
        limit: Some(10),
        offset: None,
        order_by: vec![SortField { field: "@score".to_string(), direction: SortDirection::Desc }],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let result = qe.execute_paginated(&query).unwrap();
    // All exact queries have score 1.0, so sorting by score is a no-op
    for r in &result.results {
        assert_eq!(r.score, 1.0);
    }
}

#[test]
fn test_sort_by_virtual_field_path() {
    let (engine, _temp) = setup_test_data();
    let qe = QueryEngine::new(&engine);

    let query = Query {
        path: "/people/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(20u64.to_be_bytes().to_vec()),
        })),
        limit: Some(10),
        offset: None,
        order_by: vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let result = qe.execute_paginated(&query).unwrap();
    let paths: Vec<&str> = result.results.iter().map(|r| r.file_record.path.as_str()).collect();
    for i in 1..paths.len() {
        assert!(paths[i-1] <= paths[i], "paths should be sorted asc");
    }
}

#[test]
fn test_sort_by_nonexistent_field_errors() {
    let (engine, _temp) = setup_test_data();
    let qe = QueryEngine::new(&engine);

    let query = Query {
        path: "/people/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(20u64.to_be_bytes().to_vec()),
        })),
        limit: Some(10),
        offset: None,
        order_by: vec![SortField { field: "nonexistent".to_string(), direction: SortDirection::Asc }],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let result = qe.execute_paginated(&query);
    assert!(result.is_err());
}

#[test]
fn test_sort_by_non_order_preserving_field_errors() {
    let (engine, _temp) = setup_test_data();
    // Set up a trigram index (non-order-preserving)
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    let config = PathIndexConfig {
        indexes: vec![
            IndexFieldConfig { name: "name".to_string(), index_type: "trigram".to_string(), source: None, min: None, max: None },
        ],
        parser: None,
        parser_memory_limit: None,
        logging: false,
    };
    ops.store_file(&ctx, "/trigram_test/.config/indexes.json", &config.serialize(), Some("application/json")).unwrap();
    ops.store_file(&ctx, "/trigram_test/a.json", br#"{"name":"alice"}"#, Some("application/json")).unwrap();

    let qe = QueryEngine::new(&engine);
    let query = Query {
        path: "/trigram_test/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "name".to_string(),
            operation: QueryOp::Contains("ali".to_string()),
        })),
        limit: Some(10),
        offset: None,
        order_by: vec![SortField { field: "name".to_string(), direction: SortDirection::Asc }],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let result = qe.execute_paginated(&query);
    assert!(result.is_err());
}

#[test]
fn test_no_order_by_preserves_existing_behavior() {
    let (engine, _temp) = setup_test_data();
    let qe = QueryEngine::new(&engine);

    let query = Query {
        path: "/people/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(20u64.to_be_bytes().to_vec()),
        })),
        limit: Some(10),
        offset: None,
        order_by: vec![], // no sorting
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let result = qe.execute_paginated(&query).unwrap();
    assert_eq!(result.results.len(), 10);
    // No ordering guarantee — just verify it returns results
}
```

- [ ] **Step 2: Implement sort_results**

Add to `QueryEngine`:

```rust
fn sort_results(
    &self,
    results: &mut Vec<QueryResult>,
    order_by: &[SortField],
    path: &str,
) -> EngineResult<()> {
    if order_by.is_empty() {
        return Ok(());
    }

    let index_manager = IndexManager::new(self.engine);

    // For each sort field, load the index and build a file_hash → sort_value map
    let mut sort_maps: Vec<(HashMap<Vec<u8>, Vec<u8>>, &SortField, bool)> = Vec::new();

    for sort_field in order_by {
        if sort_field.field.starts_with('@') {
            // Virtual field — no index needed, sorted from QueryResult metadata
            sort_maps.push((HashMap::new(), sort_field, true));
        } else {
            // Load index for this field
            let indexes = index_manager.load_indexes_for_field(path, &sort_field.field)?;
            let index = indexes.into_iter()
                .find(|idx| idx.converter.is_order_preserving())
                .ok_or_else(|| {
                    if index_manager.load_indexes_for_field(path, &sort_field.field)
                        .map(|v| v.is_empty()).unwrap_or(true) {
                        EngineError::NotFound(format!("No index found for sort field '{}'", sort_field.field))
                    } else {
                        EngineError::NotFound(format!(
                            "Cannot sort by non-order-preserving field '{}' — use an order-preserving index (string, numeric, timestamp)",
                            sort_field.field
                        ))
                    }
                })?;

            // Build hash → value map from the index values
            let values: HashMap<Vec<u8>, Vec<u8>> = index.values.clone();
            sort_maps.push((values, sort_field, false));
        }
    }

    // Sort results using the sort maps
    results.sort_by(|a, b| {
        for (values_map, sort_field, is_virtual) in &sort_maps {
            let cmp = if *is_virtual {
                compare_virtual_field(a, b, &sort_field.field)
            } else {
                let val_a = values_map.get(&a.file_hash).cloned().unwrap_or_default();
                let val_b = values_map.get(&b.file_hash).cloned().unwrap_or_default();
                val_a.cmp(&val_b)
            };

            let cmp = match sort_field.direction {
                SortDirection::Asc => cmp,
                SortDirection::Desc => cmp.reverse(),
            };

            if cmp != std::cmp::Ordering::Equal {
                return cmp;
            }
        }
        std::cmp::Ordering::Equal
    });

    Ok(())
}
```

Add the virtual field comparator:

```rust
fn compare_virtual_field(a: &QueryResult, b: &QueryResult, field: &str) -> std::cmp::Ordering {
    match field {
        "@score" => a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal),
        "@path" => a.file_record.path.cmp(&b.file_record.path),
        "@size" => a.file_record.total_size.cmp(&b.file_record.total_size),
        "@created_at" => a.file_record.created_at.cmp(&b.file_record.created_at),
        "@updated_at" => a.file_record.updated_at.cmp(&b.file_record.updated_at),
        _ => std::cmp::Ordering::Equal, // unknown virtual field — no-op
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test --test query_pagination_spec 2>&1`

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/engine/query_engine.rs aeordb-lib/spec/engine/query_pagination_spec.rs
git commit -m "Add sorting: single/multi-field ORDER BY, virtual @fields, order-preserving validation"
```

---

### Task 4: Cursor-based pagination

**Files:**
- Modify: `aeordb-lib/src/engine/query_engine.rs`
- Modify: `aeordb-lib/spec/engine/query_pagination_spec.rs`

- [ ] **Step 1: Write cursor tests**

Add to `query_pagination_spec.rs`:

```rust
#[test]
fn test_cursor_pagination_next_page() {
    let (engine, _temp) = setup_test_data();
    let qe = QueryEngine::new(&engine);

    // Page 1
    let query1 = Query {
        path: "/people/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(20u64.to_be_bytes().to_vec()),
        })),
        limit: Some(5),
        offset: None,
        order_by: vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let page1 = qe.execute_paginated(&query1).unwrap();
    assert_eq!(page1.results.len(), 5);
    assert!(page1.has_more);
    assert!(page1.next_cursor.is_some());

    // Page 2 using cursor
    let query2 = Query {
        path: "/people/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(20u64.to_be_bytes().to_vec()),
        })),
        limit: Some(5),
        offset: None,
        order_by: vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }],
        after: page1.next_cursor.clone(),
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let page2 = qe.execute_paginated(&query2).unwrap();
    assert_eq!(page2.results.len(), 5);

    // Pages should not overlap
    let page1_paths: Vec<&str> = page1.results.iter().map(|r| r.file_record.path.as_str()).collect();
    let page2_paths: Vec<&str> = page2.results.iter().map(|r| r.file_record.path.as_str()).collect();
    for path in &page2_paths {
        assert!(!page1_paths.contains(path), "pages should not overlap");
    }

    // Page 2 paths should be after page 1 paths (ascending order)
    assert!(page2_paths[0] > page1_paths.last().unwrap());
}

#[test]
fn test_cursor_encodes_version() {
    let (engine, _temp) = setup_test_data();
    let qe = QueryEngine::new(&engine);

    let query = Query {
        path: "/people/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(20u64.to_be_bytes().to_vec()),
        })),
        limit: Some(5),
        offset: None,
        order_by: vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let result = qe.execute_paginated(&query).unwrap();
    let cursor = result.next_cursor.unwrap();

    // Decode cursor — should contain _version
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD.decode(&cursor).unwrap();
    let cursor_json: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
    assert!(cursor_json.get("_version").is_some(), "cursor should contain _version");
    assert!(cursor_json.get("_hash").is_some(), "cursor should contain _hash");
}

#[test]
fn test_invalid_cursor_returns_error() {
    let (engine, _temp) = setup_test_data();
    let qe = QueryEngine::new(&engine);

    let query = Query {
        path: "/people/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(20u64.to_be_bytes().to_vec()),
        })),
        limit: Some(5),
        offset: None,
        order_by: vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }],
        after: Some("not-valid-base64!!!".to_string()),
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let result = qe.execute_paginated(&query);
    assert!(result.is_err());
}

#[test]
fn test_cursor_with_no_more_results() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    // Store config + 3 files (fewer than limit)
    let config = PathIndexConfig {
        indexes: vec![IndexFieldConfig { name: "name".to_string(), index_type: "string".to_string(), source: None, min: None, max: None }],
        parser: None, parser_memory_limit: None, logging: false,
    };
    ops.store_file(&ctx, "/small/.config/indexes.json", &config.serialize(), Some("application/json")).unwrap();
    for name in &["a", "b", "c"] {
        ops.store_file(&ctx, &format!("/small/{}.json", name), format!(r#"{{"name":"{}"}}"#, name).as_bytes(), Some("application/json")).unwrap();
    }

    let qe = QueryEngine::new(&engine);
    let query = Query {
        path: "/small/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "name".to_string(),
            operation: QueryOp::Gt(Vec::new()),
        })),
        limit: Some(10),
        offset: None,
        order_by: vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let result = qe.execute_paginated(&query).unwrap();
    assert!(!result.has_more);
    assert!(result.next_cursor.is_none());
}
```

- [ ] **Step 2: Implement cursor encoding/decoding**

Add to `query_engine.rs`:

```rust
use base64::Engine as _;

/// Encode a cursor from the last result's sort values and version hash.
fn encode_cursor(
    result: &QueryResult,
    order_by: &[SortField],
    sort_values: &[(&SortField, HashMap<Vec<u8>, Vec<u8>>)],
    version_hash: &[u8],
) -> String {
    let mut cursor = serde_json::Map::new();

    // Add sort field values
    for (sort_field, values_map) in sort_values {
        if sort_field.field.starts_with('@') {
            let value = match sort_field.field.as_str() {
                "@score" => serde_json::Value::from(result.score),
                "@path" => serde_json::Value::from(result.file_record.path.clone()),
                "@size" => serde_json::Value::from(result.file_record.total_size),
                "@created_at" => serde_json::Value::from(result.file_record.created_at),
                "@updated_at" => serde_json::Value::from(result.file_record.updated_at),
                _ => serde_json::Value::Null,
            };
            cursor.insert(sort_field.field.clone(), value);
        } else {
            if let Some(raw) = values_map.get(&result.file_hash) {
                cursor.insert(sort_field.field.clone(), serde_json::Value::from(hex::encode(raw)));
            }
        }
    }

    cursor.insert("_hash".to_string(), serde_json::Value::from(hex::encode(&result.file_hash)));
    cursor.insert("_version".to_string(), serde_json::Value::from(hex::encode(version_hash)));

    let json = serde_json::Value::Object(cursor);
    base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&json).unwrap_or_default())
}

/// Decode a cursor token into its components.
fn decode_cursor(cursor: &str) -> EngineResult<serde_json::Value> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(cursor)
        .map_err(|e| EngineError::JsonParseError(format!("Invalid cursor: {}", e)))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| EngineError::JsonParseError(format!("Invalid cursor JSON: {}", e)))
}
```

- [ ] **Step 3: Wire cursors into execute_paginated**

In `execute_paginated`, after sorting:

1. If `after` cursor: decode it, find the position in sorted results after the cursor's sort key + hash, skip everything before it
2. Build `next_cursor` from the last result (if `has_more`)
3. Build `prev_cursor` from the first result (if offset > 0 or cursor was used)

- [ ] **Step 4: Run tests**

Run: `cargo test --test query_pagination_spec 2>&1`

- [ ] **Step 5: Commit**

```bash
git add aeordb-lib/src/engine/query_engine.rs aeordb-lib/spec/engine/query_pagination_spec.rs
git commit -m "Add cursor-based pagination with version-locked stability"
```

---

### Task 5: HTTP API updates

**Files:**
- Modify: `aeordb-lib/src/server/engine_routes.rs`
- Create: `aeordb-lib/spec/http/query_pagination_http_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Update QueryRequest struct**

```rust
#[derive(Debug, Deserialize)]
pub struct QueryRequest {
    pub path: String,
    pub r#where: serde_json::Value,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub order_by: Option<Vec<SortFieldRequest>>,
    pub after: Option<String>,
    pub before: Option<String>,
    pub include_total: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct SortFieldRequest {
    pub field: String,
    pub direction: Option<String>, // "asc" or "desc", default "asc"
}
```

- [ ] **Step 2: Update query_endpoint to use execute_paginated**

Parse `order_by` from the request, build the Query with new fields, call `execute_paginated` instead of `execute`.

Response format: when pagination is used (order_by, offset, after, before, or default limit hit), wrap results in an envelope:

```json
{
    "results": [...],
    "has_more": true,
    "next_cursor": "...",
    "total_count": 150,
    "default_limit_hit": true,
    "default_limit": 20
}
```

When no pagination features used and all results fit within limit: return flat array (backward compatible).

- [ ] **Step 3: Write HTTP tests**

```rust
// 1. test_query_with_order_by — POST /query with order_by, verify sorted response
// 2. test_query_with_limit_and_offset — verify pagination
// 3. test_query_with_cursor — get page1, use next_cursor for page2
// 4. test_query_default_limit — no limit, verify capped at 20
// 5. test_query_response_envelope — verify has_more, next_cursor in response
// 6. test_query_include_total — verify total_count in response
// 7. test_query_backward_compat — no pagination params, flat array response
// 8. test_query_invalid_cursor — bad cursor returns 400
// 9. test_query_sort_direction — asc vs desc
// 10. test_query_virtual_field_sort — sort by @path
```

- [ ] **Step 4: Run all tests**

Run: `cargo test 2>&1 | grep "test result" | awk '{sum += $4; fail += $6} END {print "Passed:", sum, "Failed:", fail}'`

- [ ] **Step 5: Commit**

```bash
git add aeordb-lib/src/server/engine_routes.rs aeordb-lib/spec/http/query_pagination_http_spec.rs aeordb-lib/Cargo.toml
git commit -m "HTTP API: sorting, pagination, cursors, default limit, response envelope"
```

---

### Task 6: Backward compatibility + existing execute() update

**Files:**
- Modify: `aeordb-lib/src/engine/query_engine.rs`

- [ ] **Step 1: Update existing execute() to delegate**

The old `execute()` method should still work for existing callers. Make it delegate to `execute_paginated` and return just the results:

```rust
pub fn execute(&self, query: &Query) -> EngineResult<Vec<QueryResult>> {
    let paginated = self.execute_paginated(query)?;
    Ok(paginated.results)
}
```

This ensures all existing tests that call `execute()` continue to work, but now they also get the default limit applied.

- [ ] **Step 2: Verify all existing tests still pass**

Run: `cargo test 2>&1 | grep "test result" | awk '{sum += $4; fail += $6} END {print "Passed:", sum, "Failed:", fail}'`

Some existing tests may need adjusting if they expect more than 20 results without an explicit limit. Fix any that break by adding `limit: Some(100)` to their queries.

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/
git commit -m "Backward compat: execute() delegates to execute_paginated, default limit applied"
```

---

### Task 7: Update QueryBuilder for new features

**Files:**
- Modify: `aeordb-lib/src/engine/query_engine.rs`

- [ ] **Step 1: Add builder methods**

Read the existing `QueryBuilder` in query_engine.rs. Add methods:

```rust
impl<'a> QueryBuilder<'a> {
    // ... existing methods ...

    pub fn order_by(mut self, field: &str, direction: SortDirection) -> Self {
        // Add to self's order_by list
        self
    }

    pub fn offset(mut self, offset: usize) -> Self {
        // Set offset
        self
    }

    pub fn after(mut self, cursor: &str) -> Self {
        // Set after cursor
        self
    }

    pub fn before(mut self, cursor: &str) -> Self {
        // Set before cursor
        self
    }

    pub fn include_total(mut self) -> Self {
        // Set include_total = true
        self
    }

    pub fn execute_paginated(self) -> EngineResult<PaginatedResult> {
        // Build Query from self, call QueryEngine::execute_paginated
    }
}
```

- [ ] **Step 2: Write builder tests**

```rust
#[test]
fn test_builder_order_by() { ... }
#[test]
fn test_builder_offset() { ... }
#[test]
fn test_builder_chained() { ... }
```

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/src/engine/query_engine.rs
git commit -m "QueryBuilder: order_by, offset, after, before, include_total, execute_paginated"
```
