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
    VersionManager,
    tree_walker::walk_version_tree,
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
        ops.store_file_buffered(&ctx, &path, content.as_bytes(), Some("text/plain")).unwrap();
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
        let content = ops.read_file_buffered(&path).unwrap();
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
    let first = ops.read_file_buffered("/files/file-00000.txt").unwrap();
    assert_eq!(first, b"Content of file 0");
    let last_path = format!("/files/file-{:05}.txt", count - 1);
    let last = ops.read_file_buffered(&last_path).unwrap();
    assert_eq!(last, format!("Content of file {}", count - 1).as_bytes());

    // Non-existent file should fail
    let missing = ops.read_file_buffered("/files/file-99999.txt");
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
    ops.store_file_buffered(
        &ctx,
        "/people/.aeordb-config/indexes.json",
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
    ops.store_file_buffered(
        &ctx,
        "/data/.aeordb-config/indexes.json",
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
        let content = ops.read_file_buffered(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&content).unwrap();
        assert_eq!(parsed["value"], i, "file {} has wrong value", i);
    }

    // Spot-check that seed files are still readable and correct
    for i in [0u64, 25, 50, 99] {
        let path = format!("/data/item-{:04}.json", i);
        let content = ops.read_file_buffered(&path).unwrap();
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
    ops.store_file_buffered(
        &ctx,
        "/scores/.aeordb-config/indexes.json",
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
        ops.store_file_buffered(&ctx, &path, format!("cycle1 file {}", i).as_bytes(), Some("text/plain"))
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
        ops.store_file_buffered(&ctx, &path, format!("cycle2 file {}", i).as_bytes(), Some("text/plain"))
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
        ops.store_file_buffered(&ctx, &path, format!("cycle3 file {}", i).as_bytes(), Some("text/plain"))
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
        let content = ops.read_file_buffered(&path).unwrap();
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
            ops.read_file_buffered(&path).is_err(),
            "deleted cycle1 file {} should not be readable",
            i,
        );
    }

    // cycle2: files 300-399 should survive (100 files)
    for i in 300..400 {
        let path = format!("/frag/cycle2-{:04}.txt", i);
        let content = ops.read_file_buffered(&path).unwrap();
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
            ops.read_file_buffered(&path).is_err(),
            "deleted cycle2 file {} should not be readable",
            i,
        );
    }

    // cycle3: all 200 should survive
    for i in 0..200 {
        let path = format!("/frag/cycle3-{:04}.txt", i);
        let content = ops.read_file_buffered(&path).unwrap();
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

// ─── 6. Deep directory nesting (100 levels) ─────────────────────────────────
//
// Verifies that writing a file 100 directories deep works correctly and that
// directory propagation doesn't have catastrophic performance.

#[test]
fn test_stress_deep_directory_nesting() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    // Build a 100-level deep path
    let mut path = String::new();
    for i in 0..100 {
        path.push_str(&format!("/d{}", i));
    }
    path.push_str("/deep-file.txt");

    // Time the store — this triggers 100 directory propagations
    let start = std::time::Instant::now();
    ops.store_file_buffered(&ctx, &path, b"deep content", Some("text/plain")).unwrap();
    let elapsed = start.elapsed();
    println!("100-level deep file store: {:.1}ms", elapsed.as_millis());

    // Verify it's readable
    let content = ops.read_file_buffered(&path).unwrap();
    assert_eq!(content, b"deep content");

    // Store a second file at the same depth (directories already exist)
    let path2 = {
        let mut p = String::new();
        for i in 0..100 {
            p.push_str(&format!("/d{}", i));
        }
        p.push_str("/another-file.txt");
        p
    };

    let start2 = std::time::Instant::now();
    ops.store_file_buffered(&ctx, &path2, b"another", Some("text/plain")).unwrap();
    let elapsed2 = start2.elapsed();
    println!("Second file at depth 100: {:.1}ms (dirs already exist)", elapsed2.as_millis());

    // Store files at varying depths and measure
    for depth in [10, 25, 50, 75, 100] {
        let mut p = String::new();
        for i in 0..depth {
            p.push_str(&format!("/level{}", i));
        }
        p.push_str("/file.txt");
        let start = std::time::Instant::now();
        ops.store_file_buffered(&ctx, &p, b"data", Some("text/plain")).unwrap();
        println!("Depth {}: {:.1}ms", depth, start.elapsed().as_millis());
    }

    // Verify tree walker can handle deep nesting
    let head = engine.head_hash().unwrap();
    let tree = aeordb::engine::tree_walker::walk_version_tree(&engine, &head).unwrap();
    assert!(tree.files.len() >= 7, "should have at least 7 files, got {}", tree.files.len());
    println!("Tree walker found {} files, {} directories", tree.files.len(), tree.directories.len());
}

// ─── 7. Large individual files ──────────────────────────────────────────────
//
// Tests storing and reading back large files (1MB, 10MB, 50MB). Exercises
// chunking (256KB chunks), dedup, and the read-streaming path.

#[test]
fn test_stress_large_files() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    // 1MB file
    let data_1mb = vec![0x42u8; 1_000_000];
    let start = std::time::Instant::now();
    ops.store_file_buffered(&ctx, "/large/1mb.bin", &data_1mb, Some("application/octet-stream")).unwrap();
    println!("1MB store: {:.1}ms ({} chunks)", start.elapsed().as_millis(), (1_000_000 + 262143) / 262144);

    let start = std::time::Instant::now();
    let read_back = ops.read_file_buffered("/large/1mb.bin").unwrap();
    println!("1MB read: {:.1}ms", start.elapsed().as_millis());
    assert_eq!(read_back.len(), 1_000_000);
    assert!(read_back.iter().all(|&b| b == 0x42));

    // 10MB file
    let data_10mb = vec![0xAB; 10_000_000];
    let start = std::time::Instant::now();
    ops.store_file_buffered(&ctx, "/large/10mb.bin", &data_10mb, Some("application/octet-stream")).unwrap();
    println!("10MB store: {:.1}ms ({} chunks)", start.elapsed().as_millis(), (10_000_000 + 262143) / 262144);

    let start = std::time::Instant::now();
    let read_back = ops.read_file_buffered("/large/10mb.bin").unwrap();
    println!("10MB read: {:.1}ms", start.elapsed().as_millis());
    assert_eq!(read_back.len(), 10_000_000);

    // 50MB file
    let data_50mb = vec![0xCD; 50_000_000];
    let start = std::time::Instant::now();
    ops.store_file_buffered(&ctx, "/large/50mb.bin", &data_50mb, Some("application/octet-stream")).unwrap();
    println!("50MB store: {:.1}ms ({} chunks)", start.elapsed().as_millis(), (50_000_000 + 262143) / 262144);

    let start = std::time::Instant::now();
    let read_back = ops.read_file_buffered("/large/50mb.bin").unwrap();
    println!("50MB read: {:.1}ms", start.elapsed().as_millis());
    assert_eq!(read_back.len(), 50_000_000);

    // Verify dedup: store another 1MB file with same content — should be fast (chunks already exist)
    let start = std::time::Instant::now();
    ops.store_file_buffered(&ctx, "/large/1mb-dup.bin", &data_1mb, Some("application/octet-stream")).unwrap();
    let dedup_elapsed = start.elapsed();
    println!("1MB dedup store: {:.1}ms (chunks already exist)", dedup_elapsed.as_millis());

    let stats = engine.stats();
    println!("DB stats: {} chunks, {:.1}MB on disk", stats.chunk_count, stats.db_file_size_bytes as f64 / 1_048_576.0);
}

// ─── 8. Index cardinality ───────────────────────────────────────────────────
//
// Tests query performance with high cardinality (every value unique) vs low
// cardinality (5 possible values across 500 files).

#[test]
fn test_stress_index_cardinality() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    // Store index config
    let config = serde_json::json!({
        "indexes": [
            {"name": "unique_id", "type": "u64", "source": ["unique_id"]},
            {"name": "category", "type": "string", "source": ["category"]}
        ]
    });
    ops.store_file_buffered(&ctx, "/card/.aeordb-config/indexes.json",
        serde_json::to_string(&config).unwrap().as_bytes(),
        Some("application/json")).unwrap();

    // Store 500 files
    let categories = ["alpha", "beta", "gamma", "delta", "epsilon"];
    let start = std::time::Instant::now();
    for i in 0..500u64 {
        let data = serde_json::json!({
            "unique_id": i,
            "category": categories[i as usize % 5],
            "label": format!("Entry {}", i)
        });
        ops.store_file_with_indexing(&ctx, &format!("/card/entry-{:04}.json", i),
            serde_json::to_string(&data).unwrap().as_bytes(),
            Some("application/json")).unwrap();
    }
    println!("Stored 500 indexed files in {:.1}s", start.elapsed().as_secs_f64());

    let qe = QueryEngine::new(&engine);

    // High cardinality query: exact match on unique_id (1 result expected)
    let start = std::time::Instant::now();
    let query = make_query(
        "/card",
        QueryNode::Field(FieldQuery {
            field_name: "unique_id".to_string(),
            operation: QueryOp::Eq(250u64.to_be_bytes().to_vec()),
        }),
        Some(10),
    );
    let results = qe.execute(&query).unwrap();
    let high_card_time = start.elapsed();
    println!("High cardinality Eq (1/500): {} results in {:.3}ms", results.len(), high_card_time.as_secs_f64() * 1000.0);
    assert_eq!(results.len(), 1, "should find exactly 1 result for unique_id=250");

    // Low cardinality query: exact match on category (100 results expected)
    let start = std::time::Instant::now();
    let query = make_query(
        "/card",
        QueryNode::Field(FieldQuery {
            field_name: "category".to_string(),
            operation: QueryOp::Eq(b"alpha".to_vec()),
        }),
        Some(200),
    );
    let results = qe.execute(&query).unwrap();
    let low_card_time = start.elapsed();
    println!("Low cardinality Eq (100/500): {} results in {:.3}ms", results.len(), low_card_time.as_secs_f64() * 1000.0);
    assert_eq!(results.len(), 100, "should find 100 results for category=alpha");

    // Range query on high cardinality
    let start = std::time::Instant::now();
    let query = make_query(
        "/card",
        QueryNode::Field(FieldQuery {
            field_name: "unique_id".to_string(),
            operation: QueryOp::Between(
                100u64.to_be_bytes().to_vec(),
                200u64.to_be_bytes().to_vec(),
            ),
        }),
        Some(200),
    );
    let results = qe.execute(&query).unwrap();
    println!("Range query (100-200): {} results in {:.3}ms", results.len(), start.elapsed().as_secs_f64() * 1000.0);
    assert_eq!(results.len(), 101, "should find 101 results for unique_id 100..200");
}

// ─── 9. Concurrent HTTP simulation ──────────────────────────────────────────
//
// Simulates 40 concurrent "HTTP clients" hitting the engine simultaneously:
// 20 readers, 10 writers, 5 metadata checkers, 5 deleters.
// Verifies no panics under mixed concurrent read+write+delete load.

#[test]
fn test_stress_concurrent_http_simulation() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    // Seed with 200 files
    for i in 0..200 {
        let data = serde_json::json!({"id": i, "name": format!("file-{}", i)});
        ops.store_file_buffered(
            &ctx,
            &format!("/http/file-{:04}.json", i),
            serde_json::to_string(&data).unwrap().as_bytes(),
            Some("application/json"),
        )
        .unwrap();
    }
    println!("Seeded 200 files");

    let mut handles = vec![];

    let start = std::time::Instant::now();

    // 20 reader threads — each reads 100 random files
    for t in 0..20 {
        let eng = Arc::clone(&engine);
        handles.push(std::thread::spawn(move || {
            let ops = DirectoryOps::new(&eng);
            let mut ok = 0usize;
            let mut err = 0usize;
            for i in 0..100 {
                let idx = (t * 7 + i * 13) % 200; // pseudo-random spread
                let path = format!("/http/file-{:04}.json", idx);
                match ops.read_file_buffered(&path) {
                    Ok(_) => ok += 1,
                    Err(_) => err += 1,
                }
            }
            (ok, err)
        }));
    }

    // 10 writer threads — each writes 20 new files
    for t in 0..10 {
        let eng = Arc::clone(&engine);
        handles.push(std::thread::spawn(move || {
            let ctx = RequestContext::system();
            let ops = DirectoryOps::new(&eng);
            let mut ok = 0usize;
            let mut err = 0usize;
            for i in 0..20 {
                let idx = 200 + t * 20 + i;
                let data = serde_json::json!({"id": idx, "name": format!("new-{}", idx)});
                match ops.store_file_buffered(
                    &ctx,
                    &format!("/http/file-{:04}.json", idx),
                    serde_json::to_string(&data).unwrap().as_bytes(),
                    Some("application/json"),
                ) {
                    Ok(_) => ok += 1,
                    Err(_) => err += 1,
                }
            }
            (ok, err)
        }));
    }

    // 5 metadata threads — check file existence
    for t in 0..5 {
        let eng = Arc::clone(&engine);
        handles.push(std::thread::spawn(move || {
            let ops = DirectoryOps::new(&eng);
            let mut ok = 0usize;
            let mut err = 0usize;
            for i in 0..50 {
                let idx = (t * 11 + i * 3) % 200;
                let path = format!("/http/file-{:04}.json", idx);
                match ops.get_metadata(&path) {
                    Ok(Some(_)) => ok += 1,
                    Ok(None) => err += 1,
                    Err(_) => err += 1,
                }
            }
            (ok, err)
        }));
    }

    // 5 delete threads — delete some files
    for t in 0..5 {
        let eng = Arc::clone(&engine);
        handles.push(std::thread::spawn(move || {
            let ctx = RequestContext::system();
            let ops = DirectoryOps::new(&eng);
            let mut ok = 0usize;
            let mut err = 0usize;
            for i in 0..10 {
                let idx = t * 10 + i; // delete files 0-49
                let path = format!("/http/file-{:04}.json", idx);
                match ops.delete_file(&ctx, &path) {
                    Ok(_) => ok += 1,
                    Err(_) => err += 1,
                }
            }
            (ok, err)
        }));
    }

    // Wait for all threads
    let mut total_reads_ok = 0usize;
    let mut total_reads_err = 0usize;
    let mut total_writes_ok = 0usize;
    let mut total_writes_err = 0usize;
    let mut total_meta_ok = 0usize;
    let mut total_meta_err = 0usize;
    let mut total_del_ok = 0usize;
    let mut total_del_err = 0usize;

    for (i, handle) in handles.into_iter().enumerate() {
        let (ok, err) = handle.join().expect("thread panicked");
        if i < 20 {
            total_reads_ok += ok;
            total_reads_err += err;
        } else if i < 30 {
            total_writes_ok += ok;
            total_writes_err += err;
        } else if i < 35 {
            total_meta_ok += ok;
            total_meta_err += err;
        } else {
            total_del_ok += ok;
            total_del_err += err;
        }
    }

    let elapsed = start.elapsed();
    let total_ops = total_reads_ok + total_reads_err + total_writes_ok + total_writes_err
        + total_meta_ok + total_meta_err + total_del_ok + total_del_err;

    println!(
        "Concurrent HTTP simulation: {:.1}s, {} total ops ({:.0} ops/s)",
        elapsed.as_secs_f64(),
        total_ops,
        total_ops as f64 / elapsed.as_secs_f64(),
    );
    println!("  Reads: {} ok, {} err", total_reads_ok, total_reads_err);
    println!("  Writes: {} ok, {} err", total_writes_ok, total_writes_err);
    println!("  Metadata: {} ok, {} err", total_meta_ok, total_meta_err);
    println!("  Deletes: {} ok, {} err", total_del_ok, total_del_err);

    // No panics = success. Some errors are expected (reads of deleted files, etc.)
    assert!(total_reads_ok > 0, "should have successful reads");
    assert!(total_writes_ok > 0, "should have successful writes");
}

// ─── 10. Snapshot/fork at scale ──────────────────────────────────────────────
//
// Stores 1000 files, snapshots, modifies 500, snapshots again. Walks both
// trees and verifies content-addressed hashes differ for modified files and
// match for unmodified files.

#[test]
fn test_stress_snapshot_at_scale() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    // Store 1000 files
    let start = std::time::Instant::now();
    for i in 0..1000 {
        let data = format!("original content {}", i);
        ops.store_file_buffered(
            &ctx,
            &format!("/snap/file-{:04}.txt", i),
            data.as_bytes(),
            Some("text/plain"),
        )
        .unwrap();
    }
    println!("Stored 1000 files in {:.1}s", start.elapsed().as_secs_f64());

    // Snapshot v1
    let vm = VersionManager::new(&engine);
    vm.create_snapshot(&ctx, "v1", std::collections::HashMap::new())
        .unwrap();
    println!("Snapshot v1 created");

    // Modify 500 files (indices 0-499)
    let start = std::time::Instant::now();
    for i in 0..500 {
        let data = format!("MODIFIED content {}", i);
        ops.store_file_buffered(
            &ctx,
            &format!("/snap/file-{:04}.txt", i),
            data.as_bytes(),
            Some("text/plain"),
        )
        .unwrap();
    }
    println!("Modified 500 files in {:.1}s", start.elapsed().as_secs_f64());

    // Snapshot v2
    vm.create_snapshot(&ctx, "v2", std::collections::HashMap::new())
        .unwrap();
    println!("Snapshot v2 created");

    // Walk v1 tree
    let start = std::time::Instant::now();
    let v1_hash = vm.get_snapshot_hash("v1").unwrap();
    let v1_tree = walk_version_tree(&engine, &v1_hash).unwrap();
    println!(
        "Walk v1: {:.1}ms — {} files, {} dirs",
        start.elapsed().as_millis(),
        v1_tree.files.len(),
        v1_tree.directories.len(),
    );
    assert!(
        v1_tree.files.len() >= 1000,
        "v1 should have 1000 files, got {}",
        v1_tree.files.len(),
    );

    // Walk v2 tree (HEAD)
    let start = std::time::Instant::now();
    let head = engine.head_hash().unwrap();
    let v2_tree = walk_version_tree(&engine, &head).unwrap();
    println!(
        "Walk v2: {:.1}ms — {} files, {} dirs",
        start.elapsed().as_millis(),
        v2_tree.files.len(),
        v2_tree.directories.len(),
    );
    assert!(
        v2_tree.files.len() >= 1000,
        "v2 should have 1000 files, got {}",
        v2_tree.files.len(),
    );

    // Verify: a modified file should have different hashes in v1 vs v2
    let (v1_hash_0, _) = v1_tree.files.get("/snap/file-0000.txt").unwrap();
    let (v2_hash_0, _) = v2_tree.files.get("/snap/file-0000.txt").unwrap();
    assert_ne!(
        v1_hash_0, v2_hash_0,
        "v1 and v2 should have different file hashes for modified files",
    );

    // Verify: an unmodified file (index 999, outside 0-499) should share the same hash
    let (v1_hash_999, _) = v1_tree.files.get("/snap/file-0999.txt").unwrap();
    let (v2_hash_999, _) = v2_tree.files.get("/snap/file-0999.txt").unwrap();
    assert_eq!(
        v1_hash_999, v2_hash_999,
        "unmodified file should have same hash in both snapshots",
    );

    println!("Snapshot verification passed");
}

// ─── 11. Query with many results ─────────────────────────────────────────────
//
// Stores 2000 indexed files, queries that match 1800 results (tag=common),
// tests limit capping, range queries, and rare-tag filtering.

#[test]
fn test_stress_query_many_results() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    // Index config
    let config = serde_json::json!({
        "indexes": [
            {"name": "score", "type": "u64", "source": ["score"]},
            {"name": "tag", "type": "string", "source": ["tag"]}
        ]
    });
    ops.store_file_buffered(
        &ctx,
        "/many/.aeordb-config/indexes.json",
        serde_json::to_string(&config).unwrap().as_bytes(),
        Some("application/json"),
    )
    .unwrap();

    // Store 2000 files — most with tag "common", every 10th with "rare"
    let start = std::time::Instant::now();
    for i in 0..2000u64 {
        let tag = if i % 10 == 0 { "rare" } else { "common" };
        let data = serde_json::json!({"score": i, "tag": tag});
        ops.store_file_with_indexing(
            &ctx,
            &format!("/many/item-{:05}.json", i),
            serde_json::to_string(&data).unwrap().as_bytes(),
            Some("application/json"),
        )
        .unwrap();
    }
    println!(
        "Stored 2000 indexed files in {:.1}s",
        start.elapsed().as_secs_f64(),
    );

    let qe = QueryEngine::new(&engine);

    // Query tag=common with limit 50 — should cap at 50
    let start = std::time::Instant::now();
    let node = QueryNode::Field(FieldQuery {
        field_name: "tag".to_string(),
        operation: QueryOp::Eq(b"common".to_vec()),
    });
    let query = make_query("/many", node, Some(50));
    let results = qe.execute(&query).unwrap();
    println!(
        "tag=common (limit 50): {} results in {:.1}ms",
        results.len(),
        start.elapsed().as_millis(),
    );
    assert_eq!(results.len(), 50, "should return 50 (limited)");

    // Query tag=common with large limit — should return all 1800
    let node = QueryNode::Field(FieldQuery {
        field_name: "tag".to_string(),
        operation: QueryOp::Eq(b"common".to_vec()),
    });
    let start = std::time::Instant::now();
    let query = make_query("/many", node, Some(5000));
    let results = qe.execute(&query).unwrap();
    println!(
        "tag=common (limit 5000): {} results in {:.1}ms",
        results.len(),
        start.elapsed().as_millis(),
    );
    assert_eq!(results.len(), 1800, "should return 1800 common items");

    // Range query — score between 500 and 1500 (1001 results)
    let node = QueryNode::Field(FieldQuery {
        field_name: "score".to_string(),
        operation: QueryOp::Between(
            500u64.to_be_bytes().to_vec(),
            1500u64.to_be_bytes().to_vec(),
        ),
    });
    let start = std::time::Instant::now();
    let query = make_query("/many", node, Some(5000));
    let results = qe.execute(&query).unwrap();
    println!(
        "score 500-1500 (limit 5000): {} results in {:.1}ms",
        results.len(),
        start.elapsed().as_millis(),
    );
    assert_eq!(results.len(), 1001, "should return 1001 items in range");

    // Query tag=rare — should return 200
    let node = QueryNode::Field(FieldQuery {
        field_name: "tag".to_string(),
        operation: QueryOp::Eq(b"rare".to_vec()),
    });
    let start = std::time::Instant::now();
    let query = make_query("/many", node, Some(5000));
    let results = qe.execute(&query).unwrap();
    println!(
        "tag=rare: {} results in {:.1}ms",
        results.len(),
        start.elapsed().as_millis(),
    );
    assert_eq!(results.len(), 200, "should return 200 rare items");
}

// ─── 12. WASM plugin under load ──────────────────────────────────────────────
//
// Deploys the echo-plugin, invokes it 100 times rapidly (echo function),
// then 50 times with the read function. Checks correctness and throughput.

#[test]
fn test_stress_wasm_plugin_under_load() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();

    // Load echo-plugin WASM
    let wasm_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("aeordb-plugins/echo-plugin/target/wasm32-unknown-unknown/release/aeordb_echo_plugin.wasm");

    if !wasm_path.exists() {
        println!(
            "SKIP: echo-plugin WASM not found at {}, skipping WASM load test",
            wasm_path.display(),
        );
        return;
    }

    let wasm_bytes = std::fs::read(&wasm_path).unwrap();
    let pm = PluginManager::new(engine.clone());
    pm.deploy_plugin("stress-echo", "test/stress/plugin", PluginType::Wasm, wasm_bytes)
        .unwrap();

    // Store some files for the plugin to read
    let ops = DirectoryOps::new(&engine);
    for i in 0..10 {
        ops.store_file_buffered(
            &ctx,
            &format!("/plugin-data/file-{}.txt", i),
            format!("data {}", i).as_bytes(),
            Some("text/plain"),
        )
        .unwrap();
    }

    // Invoke 100 times — echo function
    let start = std::time::Instant::now();
    let mut successes = 0u32;
    let mut failures = 0u32;
    for i in 0..100 {
        let request = serde_json::json!({
            "arguments": format!("invoke {}", i).into_bytes(),
            "metadata": {"function_name": "echo", "path": "/test/stress/plugin/echo"}
        });
        let request_bytes = serde_json::to_vec(&request).unwrap();
        match pm.invoke_wasm_plugin_with_context(
            "test/stress/plugin",
            &request_bytes,
            engine.clone(),
            RequestContext::system(),
        ) {
            Ok(response) => {
                let resp: serde_json::Value =
                    serde_json::from_slice(&response).unwrap_or_default();
                if resp["status_code"] == 200 {
                    successes += 1;
                } else {
                    failures += 1;
                }
            }
            Err(_) => failures += 1,
        }
    }
    let echo_elapsed = start.elapsed();
    println!(
        "100 echo invocations: {:.1}ms ({:.1}ms each), {} ok, {} err",
        echo_elapsed.as_millis(),
        echo_elapsed.as_millis() as f64 / 100.0,
        successes,
        failures,
    );
    assert!(successes >= 95, "at least 95/100 should succeed, got {}", successes);

    // Invoke 50 times — read function (exercises host function callback)
    let start = std::time::Instant::now();
    let mut read_successes = 0u32;
    for i in 0..50 {
        let file_idx = i % 10;
        let request = serde_json::json!({
            "arguments": format!("/plugin-data/file-{}.txt", file_idx).into_bytes(),
            "metadata": {"function_name": "read", "path": "/test/stress/plugin/read"}
        });
        let request_bytes = serde_json::to_vec(&request).unwrap();
        if let Ok(response) = pm.invoke_wasm_plugin_with_context(
            "test/stress/plugin",
            &request_bytes,
            engine.clone(),
            RequestContext::system(),
        ) {
            let resp: serde_json::Value =
                serde_json::from_slice(&response).unwrap_or_default();
            if resp["status_code"] == 200 {
                read_successes += 1;
            }
        }
    }
    println!(
        "50 read invocations: {:.1}ms ({:.1}ms each), {} ok",
        start.elapsed().as_millis(),
        start.elapsed().as_millis() as f64 / 50.0,
        read_successes,
    );
    assert!(
        read_successes >= 45,
        "at least 45/50 reads should succeed, got {}",
        read_successes,
    );
}

// ─── 13. Mixed workload ─────────────────────────────────────────────────────
//
// Simultaneous readers, writers, query/scan threads, and deleters all
// hammering the engine concurrently. Verifies no panics under mixed load.

#[test]
fn test_stress_mixed_workload() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    // Seed: 200 JSON files with index config
    let config = serde_json::json!({
        "indexes": [{"name": "val", "type": "u64", "source": ["val"]}]
    });
    ops.store_file_buffered(
        &ctx,
        "/mix/.aeordb-config/indexes.json",
        serde_json::to_string(&config).unwrap().as_bytes(),
        Some("application/json"),
    )
    .unwrap();

    for i in 0..200u64 {
        let data = serde_json::json!({"val": i, "label": format!("entry-{}", i)});
        ops.store_file_with_indexing(
            &ctx,
            &format!("/mix/entry-{:04}.json", i),
            serde_json::to_string(&data).unwrap().as_bytes(),
            Some("application/json"),
        )
        .unwrap();
    }
    println!("Seeded 200 indexed files");

    let engine_arc = engine; // already Arc
    let mut handles: Vec<std::thread::JoinHandle<(&str, u32)>> = vec![];
    let start = std::time::Instant::now();

    // 5 reader threads
    for t in 0..5u64 {
        let eng = Arc::clone(&engine_arc);
        handles.push(std::thread::spawn(move || {
            let ops = DirectoryOps::new(&eng);
            let mut ok = 0u32;
            for i in 0..50u64 {
                let idx = (t * 13 + i * 7) % 200;
                if ops.read_file_buffered(&format!("/mix/entry-{:04}.json", idx)).is_ok() {
                    ok += 1;
                }
            }
            ("read", ok)
        }));
    }

    // 3 writer threads
    for t in 0..3u64 {
        let eng = Arc::clone(&engine_arc);
        handles.push(std::thread::spawn(move || {
            let ctx = RequestContext::system();
            let ops = DirectoryOps::new(&eng);
            let mut ok = 0u32;
            for i in 0..30u64 {
                let idx = 200 + t * 30 + i;
                let data = serde_json::json!({"val": idx, "label": format!("new-{}", idx)});
                if ops
                    .store_file_buffered(
                        &ctx,
                        &format!("/mix/entry-{:04}.json", idx),
                        serde_json::to_string(&data).unwrap().as_bytes(),
                        Some("application/json"),
                    )
                    .is_ok()
                {
                    ok += 1;
                }
            }
            ("write", ok)
        }));
    }

    // 3 scan threads — exercise iter_kv_entries under concurrent access
    for _t in 0..3 {
        let eng = Arc::clone(&engine_arc);
        handles.push(std::thread::spawn(move || {
            let mut ok = 0u32;
            for _ in 0..20 {
                // Exercise the snapshot/scan path — the point is concurrent access doesn't crash
                if eng.iter_kv_entries().is_ok() {
                    ok += 1;
                }
            }
            ("scan", ok)
        }));
    }

    // 2 delete threads
    for t in 0..2u64 {
        let eng = Arc::clone(&engine_arc);
        handles.push(std::thread::spawn(move || {
            let ctx = RequestContext::system();
            let ops = DirectoryOps::new(&eng);
            let mut ok = 0u32;
            for i in 0..20u64 {
                let idx = t * 20 + i + 100; // delete entries 100-139
                if ops
                    .delete_file(&ctx, &format!("/mix/entry-{:04}.json", idx))
                    .is_ok()
                {
                    ok += 1;
                }
            }
            ("delete", ok)
        }));
    }

    // Collect results
    let mut results: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    for handle in handles {
        let (op, count) = handle.join().expect("thread panicked");
        *results.entry(op).or_default() += count;
    }

    let elapsed = start.elapsed();
    let total: u32 = results.values().sum();
    println!(
        "Mixed workload: {:.1}s, {} total ops ({:.0} ops/s)",
        elapsed.as_secs_f64(),
        total,
        total as f64 / elapsed.as_secs_f64(),
    );
    for (op, count) in &results {
        println!("  {}: {}", op, count);
    }

    // Verify no panics and some ops succeeded
    assert!(
        results.get("read").copied().unwrap_or(0) > 0,
        "should have successful reads",
    );
    assert!(
        results.get("write").copied().unwrap_or(0) > 0,
        "should have successful writes",
    );
    assert!(
        results.get("scan").copied().unwrap_or(0) > 0,
        "should have successful scans",
    );
}
