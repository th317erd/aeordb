// Stress tests for AeorDB: large file counts, query performance, concurrency,
// WASM consistency, and fragmentation/GC cycles.
//
// Run with: cargo test --test stress_test_spec -- --test-threads=1 --nocapture

use std::sync::Arc;

use aeordb::engine::{
    DirectoryOps, RequestContext,
    query_engine::{
        QueryEngine, Query, QueryNode, FieldQuery, QueryOp,
        QueryStrategy, ExplainMode,
    },
    gc::run_gc,
};
use aeordb::plugins::plugin_manager::PluginManager;
use aeordb::plugins::types::PluginType;
use aeordb::server::create_temp_engine_for_tests;

// ─── Helper ─────────────────────────────────────────────────────────────────

fn make_query(path: &str, node: QueryNode, limit: Option<usize>) -> Query {
    Query {
        path: path.to_string(),
        field_queries: vec![],
        node: Some(node),
        limit,
        offset: None,
        order_by: vec![],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
        aggregate: None,
        explain: ExplainMode::Off,
    }
}

// ─── 1. Massive number of small files ────────────────────────────────────────
//
// Stores many small files in a flat directory, verifies random reads, and
// checks stats. Count is tuned for debug-mode performance (~2000 files).

#[test]
fn test_stress_many_small_files() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    let count = 2_000usize;

    let start = std::time::Instant::now();
    for i in 0..count {
        let path = format!("/files/file-{:05}.txt", i);
        let content = format!("Content of file {}", i);
        ops.store_file(&ctx, &path, content.as_bytes(), Some("text/plain")).unwrap();
    }
    let store_elapsed = start.elapsed();
    println!(
        "Stored {} files in {:.2}s ({:.0} files/sec)",
        count,
        store_elapsed.as_secs_f64(),
        count as f64 / store_elapsed.as_secs_f64(),
    );

    // Verify random reads — pick every 20th file (100 reads total)
    let read_start = std::time::Instant::now();
    let mut reads = 0usize;
    for i in (0..count).step_by(20) {
        let path = format!("/files/file-{:05}.txt", i);
        let content = ops.read_file(&path).unwrap();
        let expected = format!("Content of file {}", i);
        assert_eq!(content, expected.as_bytes(), "mismatch at file {}", i);
        reads += 1;
    }
    let read_elapsed = read_start.elapsed();
    println!(
        "Read {} files in {:.2}s ({:.0} reads/sec)",
        reads,
        read_elapsed.as_secs_f64(),
        reads as f64 / read_elapsed.as_secs_f64(),
    );

    let stats = engine.stats();
    assert!(
        stats.file_count >= count,
        "expected {} files, got {}",
        count,
        stats.file_count,
    );

    // Boundary checks: first and last file
    let first = ops.read_file("/files/file-00000.txt").unwrap();
    assert_eq!(first, b"Content of file 0");
    let last_path = format!("/files/file-{:05}.txt", count - 1);
    let last = ops.read_file(&last_path).unwrap();
    assert_eq!(last, format!("Content of file {}", count - 1).as_bytes());

    // Non-existent file should fail
    let missing = ops.read_file("/files/file-99999.txt");
    assert!(missing.is_err(), "reading non-existent file should fail");
}

// ─── 2. Query against indexed dataset ────────────────────────────────────────
//
// Stores JSON files with indexed fields, then runs Eq, Between, and Contains
// queries. Measures query performance.

#[test]
fn test_stress_query_indexed_files() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    // Create index config
    let config = serde_json::json!({
        "indexes": [
            {"name": "age", "type": "u64", "source": ["age"]},
            {"name": "name", "type": "trigram", "source": ["name"]},
            {"name": "city", "type": "string", "source": ["city"]}
        ]
    });
    ops.store_file(
        &ctx,
        "/people/.config/indexes.json",
        serde_json::to_string(&config).unwrap().as_bytes(),
        Some("application/json"),
    )
    .unwrap();

    let cities = [
        "New York", "London", "Tokyo", "Paris", "Berlin",
        "Sydney", "Toronto", "Mumbai", "Beijing", "Cairo",
    ];

    let file_count = 200usize;
    let start = std::time::Instant::now();
    for i in 0..file_count {
        let path = format!("/people/person-{:05}.json", i);
        let data = serde_json::json!({
            "name": format!("Person Number {}", i),
            "age": 20 + (i % 60),
            "city": cities[i % cities.len()],
            "email": format!("person{}@example.com", i)
        });
        ops.store_file_with_indexing(
            &ctx,
            &path,
            serde_json::to_string(&data).unwrap().as_bytes(),
            Some("application/json"),
        )
        .unwrap();
    }
    let store_elapsed = start.elapsed();
    println!(
        "Stored {} indexed files in {:.2}s ({:.0} files/sec)",
        file_count,
        store_elapsed.as_secs_f64(),
        file_count as f64 / store_elapsed.as_secs_f64(),
    );

    let qe = QueryEngine::new(&engine);

    // Query: exact match on city = "Tokyo"
    let query_start = std::time::Instant::now();
    let query = make_query(
        "/people/",
        QueryNode::Field(FieldQuery {
            field_name: "city".to_string(),
            operation: QueryOp::Eq(b"Tokyo".to_vec()),
        }),
        Some(500),
    );
    let results = qe.execute(&query).unwrap();
    let query_elapsed = query_start.elapsed();
    println!(
        "Eq(city=Tokyo) on {} files: {} results in {:.3}s",
        file_count,
        results.len(),
        query_elapsed.as_secs_f64(),
    );
    assert!(!results.is_empty(), "should find some Tokyo people");
    // Tokyo is every 10th city = ~20 out of 200
    assert!(
        results.len() >= 10,
        "expected ~20 Tokyo people, got {}",
        results.len(),
    );

    // Query: range on age [25, 30]
    let range_start = std::time::Instant::now();
    let range_query = make_query(
        "/people/",
        QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Between(
                25u64.to_be_bytes().to_vec(),
                30u64.to_be_bytes().to_vec(),
            ),
        }),
        Some(500),
    );
    let range_results = qe.execute(&range_query).unwrap();
    let range_elapsed = range_start.elapsed();
    println!(
        "Between(age 25..30) on {} files: {} results in {:.3}s",
        file_count,
        range_results.len(),
        range_elapsed.as_secs_f64(),
    );
    assert!(!range_results.is_empty(), "should find people aged 25-30");

    // Query: contains on name (trigram)
    let contains_start = std::time::Instant::now();
    let contains_query = make_query(
        "/people/",
        QueryNode::Field(FieldQuery {
            field_name: "name".to_string(),
            operation: QueryOp::Contains("Number 42".to_string()),
        }),
        Some(500),
    );
    let contains_results = qe.execute(&contains_query).unwrap();
    let contains_elapsed = contains_start.elapsed();
    println!(
        "Contains(name~'Number 42') on {} files: {} results in {:.3}s",
        file_count,
        contains_results.len(),
        contains_elapsed.as_secs_f64(),
    );
    // "Number 42" matches person 42, 420-429 — should be at least 1
    assert!(!contains_results.is_empty(), "should find people matching 'Number 42'");

    // Query for a city that doesn't exist — should return 0 results
    let empty_query = make_query(
        "/people/",
        QueryNode::Field(FieldQuery {
            field_name: "city".to_string(),
            operation: QueryOp::Eq(b"Atlantis".to_vec()),
        }),
        Some(10),
    );
    let empty_results = qe.execute(&empty_query).unwrap();
    assert!(
        empty_results.is_empty(),
        "Atlantis query should return 0 results, got {}",
        empty_results.len(),
    );
}

// ─── 3. Concurrent writes during queries ────────────────────────────────────
//
// Seeds files with an index, then runs queries and writes simultaneously
// from multiple threads to verify no panics or data corruption.

#[test]
fn test_stress_concurrent_writes_during_queries() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    // Seed 500 JSON files with index
    let config = serde_json::json!({
        "indexes": [{"name": "value", "type": "u64", "source": ["value"]}]
    });
    ops.store_file(
        &ctx,
        "/data/.config/indexes.json",
        serde_json::to_string(&config).unwrap().as_bytes(),
        Some("application/json"),
    )
    .unwrap();

    let seed_count = 100u64;
    for i in 0..seed_count {
        let path = format!("/data/item-{:04}.json", i);
        let data = serde_json::json!({"value": i, "label": format!("Item {}", i)});
        ops.store_file_with_indexing(
            &ctx,
            &path,
            serde_json::to_string(&data).unwrap().as_bytes(),
            Some("application/json"),
        )
        .unwrap();
    }

    let mut handles = vec![];

    // 4 query threads — continuously querying
    for t in 0..4u64 {
        let eng = Arc::clone(&engine);
        handles.push(std::thread::spawn(move || {
            let qe = QueryEngine::new(&eng);
            let mut found = 0usize;
            for round in 0..10u64 {
                let target = (t * 10 + round) % seed_count;
                let query = make_query(
                    "/data/",
                    QueryNode::Field(FieldQuery {
                        field_name: "value".to_string(),
                        operation: QueryOp::Eq(target.to_be_bytes().to_vec()),
                    }),
                    Some(10),
                );
                if let Ok(results) = qe.execute(&query) {
                    found += results.len();
                }
            }
            found
        }));
    }

    // 1 writer thread — adding 50 new files
    let new_start = seed_count;
    let new_end = seed_count + 50;
    {
        let eng = Arc::clone(&engine);
        handles.push(std::thread::spawn(move || {
            let ctx = RequestContext::system();
            let ops = DirectoryOps::new(&eng);
            let mut written = 0usize;
            for i in new_start..new_end {
                let path = format!("/data/item-{:04}.json", i);
                let data =
                    serde_json::json!({"value": i, "label": format!("New Item {}", i)});
                ops.store_file_with_indexing(
                    &ctx,
                    &path,
                    serde_json::to_string(&data).unwrap().as_bytes(),
                    Some("application/json"),
                )
                .unwrap();
                written += 1;
            }
            written
        }));
    }

    for h in handles {
        h.join().expect("thread panicked");
    }

    // Verify: all seed + new files should exist
    let expected_total = (seed_count + 50) as usize;
    let stats = engine.stats();
    assert!(
        stats.file_count >= expected_total,
        "expected at least {} files, got {}",
        expected_total,
        stats.file_count,
    );

    // Spot-check the new files
    let ops = DirectoryOps::new(&engine);
    for i in [new_start, new_start + 10, new_start + 25, new_start + 40, new_end - 1] {
        let path = format!("/data/item-{:04}.json", i);
        let content = ops.read_file(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&content).unwrap();
        assert_eq!(parsed["value"], i, "file {} has wrong value", i);
    }

    // Spot-check that seed files are still readable and correct
    for i in [0u64, 25, 50, 99] {
        let path = format!("/data/item-{:04}.json", i);
        let content = ops.read_file(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&content).unwrap();
        assert_eq!(parsed["value"], i, "seed file {} has wrong value", i);
    }
}

// ─── 4. WASM query vs direct query consistency ──────────────────────────────
//
// Stores indexed files, runs a direct query, then deploys the echo-plugin
// and verifies WASM invocation works. Skips gracefully if the WASM binary
// is not found.

#[test]
fn test_stress_wasm_query_vs_direct_query() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    // Store indexed files
    let config = serde_json::json!({
        "indexes": [{"name": "score", "type": "u64", "source": ["score"]}]
    });
    ops.store_file(
        &ctx,
        "/scores/.config/indexes.json",
        serde_json::to_string(&config).unwrap().as_bytes(),
        Some("application/json"),
    )
    .unwrap();

    for i in 0..100u64 {
        let path = format!("/scores/entry-{:03}.json", i);
        let data = serde_json::json!({"score": i * 10, "player": format!("Player {}", i)});
        ops.store_file_with_indexing(
            &ctx,
            &path,
            serde_json::to_string(&data).unwrap().as_bytes(),
            Some("application/json"),
        )
        .unwrap();
    }

    // Direct query: score between 100 and 200
    let qe = QueryEngine::new(&engine);
    let query = make_query(
        "/scores/",
        QueryNode::Field(FieldQuery {
            field_name: "score".to_string(),
            operation: QueryOp::Between(
                100u64.to_be_bytes().to_vec(),
                200u64.to_be_bytes().to_vec(),
            ),
        }),
        Some(50),
    );
    let direct_results = qe.execute(&query).unwrap();
    let direct_count = direct_results.len();
    assert!(direct_count > 0, "direct query should find results with score 100..200");
    println!("Direct query: {} results for score 100..200", direct_count);

    // Run the same query a second time to verify consistency
    let direct_results2 = qe.execute(&query).unwrap();
    assert_eq!(
        direct_results.len(),
        direct_results2.len(),
        "repeated query should return same count",
    );

    // WASM query (via plugin manager)
    let wasm_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("aeordb-plugins/echo-plugin/target/wasm32-unknown-unknown/release/aeordb_echo_plugin.wasm");

    if !wasm_path.exists() {
        println!(
            "SKIP: echo-plugin WASM not found at {}, skipping WASM comparison",
            wasm_path.display()
        );
        return;
    }

    let pm = PluginManager::new(engine.clone());
    let wasm_bytes = std::fs::read(&wasm_path).unwrap();
    pm.deploy_plugin("query-test", "test/query/plugin", PluginType::Wasm, wasm_bytes)
        .unwrap();

    // Invoke echo function to verify WASM is working
    let request = serde_json::json!({
        "arguments": [],
        "metadata": {"function_name": "echo", "path": "/test/query/plugin/echo"}
    });
    let response = pm
        .invoke_wasm_plugin_with_context(
            "test/query/plugin",
            &serde_json::to_vec(&request).unwrap(),
            engine.clone(),
            RequestContext::system(),
        )
        .unwrap();

    let resp: serde_json::Value = serde_json::from_slice(&response).unwrap();
    assert_eq!(resp["status_code"], 200, "WASM invoke should succeed");

    println!(
        "Direct query: {} results, WASM invoke: OK (status {})",
        direct_count, resp["status_code"]
    );
}

// ─── 5. Fragmentation stress (create-delete-create cycles) ──────────────────
//
// Multiple cycles of create/delete, then GC, then verify all surviving files
// are readable and GC is idempotent.

#[test]
fn test_stress_fragmentation_create_delete_cycles() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    let mut total_created = 0u64;
    let mut total_deleted = 0u64;

    // Cycle 1: Create 200, delete 120
    for i in 0..200 {
        let path = format!("/frag/cycle1-{:04}.txt", i);
        ops.store_file(&ctx, &path, format!("cycle1 file {}", i).as_bytes(), Some("text/plain"))
            .unwrap();
        total_created += 1;
    }
    for i in 0..120 {
        let path = format!("/frag/cycle1-{:04}.txt", i);
        ops.delete_file(&ctx, &path).unwrap();
        total_deleted += 1;
    }
    println!("Cycle 1: created 200, deleted 120");

    // Cycle 2: Create 400, delete 300
    for i in 0..400 {
        let path = format!("/frag/cycle2-{:04}.txt", i);
        ops.store_file(&ctx, &path, format!("cycle2 file {}", i).as_bytes(), Some("text/plain"))
            .unwrap();
        total_created += 1;
    }
    for i in 0..300 {
        let path = format!("/frag/cycle2-{:04}.txt", i);
        ops.delete_file(&ctx, &path).unwrap();
        total_deleted += 1;
    }
    println!("Cycle 2: created 400, deleted 300");

    // Cycle 3: Create 200 more (should reuse some void space)
    for i in 0..200 {
        let path = format!("/frag/cycle3-{:04}.txt", i);
        ops.store_file(&ctx, &path, format!("cycle3 file {}", i).as_bytes(), Some("text/plain"))
            .unwrap();
        total_created += 1;
    }
    println!("Cycle 3: created 200 more");

    let stats_before_gc = engine.stats();
    println!(
        "Before GC: {} files, {} voids, {} void bytes, DB size: {} bytes",
        stats_before_gc.file_count,
        stats_before_gc.void_count,
        stats_before_gc.void_space_bytes,
        stats_before_gc.db_file_size_bytes,
    );

    // Run GC
    let gc_result = run_gc(&engine, &ctx, false).unwrap();
    println!(
        "GC: {} garbage entries, {} bytes reclaimed",
        gc_result.garbage_entries, gc_result.reclaimed_bytes,
    );

    let stats_after_gc = engine.stats();
    println!(
        "After GC: {} files, {} voids, {} void bytes, DB size: {} bytes",
        stats_after_gc.file_count,
        stats_after_gc.void_count,
        stats_after_gc.void_space_bytes,
        stats_after_gc.db_file_size_bytes,
    );

    // Verify remaining files are readable
    // cycle1: files 120-199 should survive (80 files)
    for i in 120..200 {
        let path = format!("/frag/cycle1-{:04}.txt", i);
        let content = ops.read_file(&path).unwrap();
        assert_eq!(
            content,
            format!("cycle1 file {}", i).as_bytes(),
            "cycle1 file {} mismatch",
            i,
        );
    }

    // cycle1: deleted files should NOT be readable
    for i in [0, 50, 119] {
        let path = format!("/frag/cycle1-{:04}.txt", i);
        assert!(
            ops.read_file(&path).is_err(),
            "deleted cycle1 file {} should not be readable",
            i,
        );
    }

    // cycle2: files 300-399 should survive (100 files)
    for i in 300..400 {
        let path = format!("/frag/cycle2-{:04}.txt", i);
        let content = ops.read_file(&path).unwrap();
        assert_eq!(
            content,
            format!("cycle2 file {}", i).as_bytes(),
            "cycle2 file {} mismatch",
            i,
        );
    }

    // cycle2: deleted files should NOT be readable
    for i in [0, 150, 299] {
        let path = format!("/frag/cycle2-{:04}.txt", i);
        assert!(
            ops.read_file(&path).is_err(),
            "deleted cycle2 file {} should not be readable",
            i,
        );
    }

    // cycle3: all 200 should survive
    for i in 0..200 {
        let path = format!("/frag/cycle3-{:04}.txt", i);
        let content = ops.read_file(&path).unwrap();
        assert_eq!(
            content,
            format!("cycle3 file {}", i).as_bytes(),
            "cycle3 file {} mismatch",
            i,
        );
    }

    // Expected surviving files: 80 + 100 + 200 = 380
    assert!(
        stats_after_gc.file_count >= 380,
        "expected ~380 surviving files, got {}",
        stats_after_gc.file_count,
    );

    // GC should have found garbage from delete cycles
    assert!(
        gc_result.garbage_entries > 0,
        "GC should collect garbage from delete cycles",
    );

    // Second GC should find nothing (idempotent)
    let gc2 = run_gc(&engine, &ctx, false).unwrap();
    assert_eq!(
        gc2.garbage_entries, 0,
        "second GC should find 0 garbage, found {}",
        gc2.garbage_entries,
    );

    println!(
        "Total: created {}, deleted {}, surviving ~{}",
        total_created, total_deleted, stats_after_gc.file_count,
    );
}
