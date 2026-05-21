// AeorDB Release-Mode Stress Benchmark
//
// Run with: cargo run --release --bin stress-benchmark -p aeordb
//
// This is a standalone binary (NOT a cargo test) so it runs in release mode
// with full optimizations. It stress-tests the engine with as many files as
// possible within the time budget (10 minutes for Phase 1).

use std::sync::Arc;
use std::time::{Duration, Instant};

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::gc::run_gc;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::query_engine::{
    ExplainMode, FieldQuery, Query, QueryEngine, QueryNode, QueryOp, QueryStrategy,
};
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::RequestContext;

// ─── Constants ──────────────────────────────────────────────────────────────

/// Maximum files to attempt in Phase 1.
const TARGET_FILES: usize = 1_000_000;
/// Shards in the /s-XXXX/ directories (flat, one level under root).
const SHARD_COUNT: usize = 10_000;
/// Files per shard directory.
const FILES_PER_SHARD: usize = 100;
/// Print a checkpoint every N files.
const CHECKPOINT_INTERVAL: usize = 5_000;
/// No timeout — run until all files are stored.
const PHASE1_TIMEOUT: Duration = Duration::from_secs(86400); // 24 hours

const INDEXED_FILE_COUNT: usize = 10_000;
const CONCURRENT_READER_THREADS: usize = 8;
const VERIFY_COUNT: usize = 1_000;

// ─── Helpers ────────────────────────────────────────────────────────────────

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

/// Simple deterministic pseudo-random number generator (xorshift64).
struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    fn new(seed: u64) -> Self {
        SimpleRng {
            state: if seed == 0 { 1 } else { seed },
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn next_usize(&mut self, max: usize) -> usize {
        (self.next_u64() % max as u64) as usize
    }

    fn shuffle<T>(&mut self, slice: &mut [T]) {
        for i in (1..slice.len()).rev() {
            let j = self.next_usize(i + 1);
            slice.swap(i, j);
        }
    }
}

fn file_path(shard: usize, file: usize) -> String {
    format!("/s-{:04}/f-{:03}.txt", shard, file)
}

fn file_content(shard: usize, file: usize) -> Vec<u8> {
    format!("data-{}-{}", shard, file).into_bytes()
}

fn print_separator() {
    eprintln!("{}", "=".repeat(72));
}

fn log_progress(msg: &str) {
    eprintln!("{}", msg);
    // Also write to a file so external tools can monitor progress
    // (stderr may be buffered when piped through cargo)
    let _ = std::fs::write("/tmp/aeordb-bench-progress.txt", msg);
}

// ─── Phase 1: Bulk File Storage ─────────────────────────────────────────────

fn phase1_bulk_store(engine: &StorageEngine, ctx: &RequestContext) -> usize {
    log_progress(&format!(
        "\n--- PHASE 1: Bulk File Storage (target: {} files, timeout: {}s) ---",
        TARGET_FILES,
        PHASE1_TIMEOUT.as_secs()
    ));

    let ops = DirectoryOps::new(engine);
    let start = Instant::now();
    let mut stored = 0usize;
    let mut last_checkpoint = Instant::now();

    'outer: for shard in 0..SHARD_COUNT {
        for file in 0..FILES_PER_SHARD {
            let path = file_path(shard, file);
            let content = file_content(shard, file);
            if let Err(e) = ops.store_file_buffered(ctx, &path, &content, Some("text/plain")) {
                log_progress(&format!("  ERROR storing {}: {}", path, e));
                break 'outer;
            }
            stored += 1;

            // Timeout check every 100 files (cheap Instant::now call)
            if stored.is_multiple_of(100) && start.elapsed() > PHASE1_TIMEOUT {
                log_progress(&format!(
                    "  TIMEOUT after {:.1}s with {} files stored",
                    start.elapsed().as_secs_f64(),
                    stored
                ));
                break 'outer;
            }

            if stored.is_multiple_of(CHECKPOINT_INTERVAL) {
                let elapsed = start.elapsed();
                let rate = stored as f64 / elapsed.as_secs_f64();
                let chunk_rate =
                    CHECKPOINT_INTERVAL as f64 / last_checkpoint.elapsed().as_secs_f64();
                log_progress(&format!(
                    "  [{:>7} / {}] {:.1}s | {:.0} files/s overall | {:.0} files/s chunk",
                    stored, TARGET_FILES, elapsed.as_secs_f64(), rate, chunk_rate
                ));
                last_checkpoint = Instant::now();
            }
        }
    }

    let elapsed = start.elapsed();
    let rate = if elapsed.as_secs_f64() > 0.0 {
        stored as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };

    let stats = engine.stats();
    log_progress(&format!(
        "  Phase 1 DONE: {} files in {:.2}s ({:.0} files/s)",
        stored,
        elapsed.as_secs_f64(),
        rate
    ));
    log_progress(&format!(
        "  DB stats: {} entries, {} files, {} dirs, {:.1} MB on disk",
        stats.entry_count,
        stats.file_count,
        stats.directory_count,
        stats.db_file_size_bytes as f64 / (1024.0 * 1024.0)
    ));
    print_separator();

    stored
}

// ─── Phase 2: Random Read Benchmark ─────────────────────────────────────────

fn phase2_random_reads(engine: &StorageEngine, total_stored: usize) {
    let read_count = 10_000.min(total_stored);
    log_progress(&format!(
        "\n--- PHASE 2: Random Read Benchmark ({} reads) ---",
        read_count
    ));

    let ops = DirectoryOps::new(engine);
    let mut rng = SimpleRng::new(42);

    let paths: Vec<(usize, usize)> = (0..read_count)
        .map(|_| {
            let flat = rng.next_usize(total_stored);
            (flat / FILES_PER_SHARD, flat % FILES_PER_SHARD)
        })
        .collect();

    let start = Instant::now();
    let mut success = 0usize;
    let mut fail = 0usize;

    for &(shard, file) in &paths {
        let path = file_path(shard, file);
        match ops.read_file_buffered(&path) {
            Ok(data) => {
                let expected = file_content(shard, file);
                if data == expected {
                    success += 1;
                } else {
                    fail += 1;
                    if fail <= 3 {
                        eprintln!(
                            "  MISMATCH at {}: got {} bytes, expected {} bytes",
                            path,
                            data.len(),
                            expected.len()
                        );
                    }
                }
            }
            Err(e) => {
                fail += 1;
                if fail <= 3 {
                    eprintln!("  READ ERROR at {}: {}", path, e);
                }
            }
        }
    }

    let elapsed = start.elapsed();
    let rate = read_count as f64 / elapsed.as_secs_f64();

    log_progress(&format!(
        "  {} reads in {:.2}s ({:.0} reads/s) -- {} ok, {} failed",
        read_count,
        elapsed.as_secs_f64(),
        rate,
        success,
        fail
    ));
    print_separator();
}

// ─── Phase 3: Query Benchmark ───────────────────────────────────────────────

fn phase3_query_benchmark(engine: &StorageEngine, ctx: &RequestContext) {
    log_progress(&format!(
        "\n--- PHASE 3: Query Benchmark ({} indexed files) ---",
        INDEXED_FILE_COUNT
    ));

    let ops = DirectoryOps::new(engine);

    // Store index config
    let config = PathIndexConfig {
        parser: None,
        parser_memory_limit: None,
        logging: false,
        glob: None,

        indexes: vec![
            IndexFieldConfig {
                name: "age".to_string(),
                index_type: "u64".to_string(),
                source: None,
                min: Some(0.0),
                max: Some(200.0),
            },
            IndexFieldConfig {
                name: "city".to_string(),
                index_type: "string".to_string(),
                source: None,
                min: None,
                max: None,
            },
        ],
    };
    let config_data = config.serialize();
    ops.store_file_buffered(
        ctx,
        "/indexed/.config/indexes.json",
        &config_data,
        Some("application/json"),
    )
    .expect("store index config");

    // Store indexed JSON files
    let store_start = Instant::now();
    for i in 0..INDEXED_FILE_COUNT {
        let name = format!("Person{}", i);
        let age = i % 80;
        let city = format!("City{}", i % 50);
        let json = format!(
            r#"{{"name":"{}","age":{},"city":"{}"}}"#,
            name, age, city
        );
        let path = format!("/indexed/person-{:05}.json", i);
        if let Err(e) =
            ops.store_file_with_indexing(ctx, &path, json.as_bytes(), Some("application/json"))
        {
            eprintln!("  ERROR storing indexed file {}: {}", path, e);
        }

        if (i + 1) % 500 == 0 {
            log_progress(&format!(
                "  Stored {} / {} indexed files ({:.1}s)",
                i + 1,
                INDEXED_FILE_COUNT,
                store_start.elapsed().as_secs_f64()
            ));
        }
    }
    log_progress(&format!(
        "  Indexed file storage: {:.2}s ({:.0} files/s)",
        store_start.elapsed().as_secs_f64(),
        INDEXED_FILE_COUNT as f64 / store_start.elapsed().as_secs_f64()
    ));

    let qe = QueryEngine::new(engine);

    // Query 1: Eq on age
    {
        let start = Instant::now();
        let query = make_query(
            "/indexed",
            QueryNode::Field(FieldQuery {
                field_name: "age".to_string(),
                operation: QueryOp::Eq(25u64.to_be_bytes().to_vec()),
            }),
            Some(1000),
        );
        let results = qe.execute(&query).expect("Eq query failed");
        let elapsed = start.elapsed();
        log_progress(&format!(
            "  Eq(age=25): {} results in {:.3}ms",
            results.len(),
            elapsed.as_secs_f64() * 1000.0
        ));
    }

    // Query 2: Between on age
    {
        let start = Instant::now();
        let query = make_query(
            "/indexed",
            QueryNode::Field(FieldQuery {
                field_name: "age".to_string(),
                operation: QueryOp::Between(
                    20u64.to_be_bytes().to_vec(),
                    30u64.to_be_bytes().to_vec(),
                ),
            }),
            Some(5000),
        );
        let results = qe.execute(&query).expect("Between query failed");
        let elapsed = start.elapsed();
        log_progress(&format!(
            "  Between(age 20..30): {} results in {:.3}ms",
            results.len(),
            elapsed.as_secs_f64() * 1000.0
        ));
    }

    // Query 3: Eq on city (string)
    {
        let start = Instant::now();
        let query = make_query(
            "/indexed",
            QueryNode::Field(FieldQuery {
                field_name: "city".to_string(),
                operation: QueryOp::Eq(b"City5".to_vec()),
            }),
            Some(1000),
        );
        let results = qe.execute(&query).expect("city Eq query failed");
        let elapsed = start.elapsed();
        log_progress(&format!(
            "  Eq(city='City5'): {} results in {:.3}ms",
            results.len(),
            elapsed.as_secs_f64() * 1000.0
        ));
    }

    // Query 4: And(age=25, city=City25)
    {
        let start = Instant::now();
        let query = make_query(
            "/indexed",
            QueryNode::And(vec![
                QueryNode::Field(FieldQuery {
                    field_name: "age".to_string(),
                    operation: QueryOp::Eq(25u64.to_be_bytes().to_vec()),
                }),
                QueryNode::Field(FieldQuery {
                    field_name: "city".to_string(),
                    operation: QueryOp::Eq(b"City25".to_vec()),
                }),
            ]),
            Some(1000),
        );
        let results = qe.execute(&query).expect("And query failed");
        let elapsed = start.elapsed();
        log_progress(&format!(
            "  And(age=25, city='City25'): {} results in {:.3}ms",
            results.len(),
            elapsed.as_secs_f64() * 1000.0
        ));
    }

    print_separator();
}

// ─── Phase 4: Concurrent Read + Write ───────────────────────────────────────

fn phase4_concurrent(engine: &Arc<StorageEngine>, total_stored: usize) {
    // Scale concurrent writes to something achievable (30s max for writer)
    let concurrent_writes = 1_000;
    let reads_per_thread = 1_000.min(total_stored / CONCURRENT_READER_THREADS.max(1));

    log_progress(&format!(
        "\n--- PHASE 4: Concurrent ({} readers x {} reads + 1 writer x {} writes) ---",
        CONCURRENT_READER_THREADS, reads_per_thread, concurrent_writes
    ));

    let start = Instant::now();
    let mut handles = Vec::new();

    // Spawn reader threads
    for thread_id in 0..CONCURRENT_READER_THREADS {
        let eng = Arc::clone(engine);
        let handle = std::thread::spawn(move || {
            let ops = DirectoryOps::new(&eng);
            let mut rng = SimpleRng::new(100 + thread_id as u64);
            let mut ok = 0usize;
            let mut err = 0usize;

            for _ in 0..reads_per_thread {
                let flat = rng.next_usize(total_stored);
                let shard = flat / FILES_PER_SHARD;
                let file = flat % FILES_PER_SHARD;
                let path = file_path(shard, file);
                match ops.read_file_buffered(&path) {
                    Ok(_) => ok += 1,
                    Err(_) => err += 1,
                }
            }
            (ok, err)
        });
        handles.push(handle);
    }

    // Spawn writer thread
    let eng_w = Arc::clone(engine);
    let writer_handle = std::thread::spawn(move || {
        let ops = DirectoryOps::new(&eng_w);
        let ctx = RequestContext::system();
        let mut written = 0usize;
        for i in 0..concurrent_writes {
            let path = format!("/concurrent/file-{:05}.txt", i);
            let content = format!("concurrent-data-{}", i);
            if ops
                .store_file_buffered(&ctx, &path, content.as_bytes(), Some("text/plain"))
                .is_ok()
            {
                written += 1;
            }
        }
        written
    });

    // Collect results
    let mut total_reads_ok = 0usize;
    let mut total_reads_err = 0usize;
    for h in handles {
        let (ok, err) = h.join().expect("reader thread panicked");
        total_reads_ok += ok;
        total_reads_err += err;
    }
    let written = writer_handle.join().expect("writer thread panicked");

    let elapsed = start.elapsed();
    let total_ops = total_reads_ok + total_reads_err + written;
    let rate = total_ops as f64 / elapsed.as_secs_f64();

    log_progress(&format!(
        "  Concurrent phase: {:.2}s total",
        elapsed.as_secs_f64()
    ));
    log_progress(&format!(
        "  Reads: {} ok, {} errors  |  Writes: {} files",
        total_reads_ok, total_reads_err, written
    ));
    log_progress(&format!("  Combined throughput: {:.0} ops/s", rate));
    print_separator();
}

// ─── Phase 5: GC After Deletes ─────────────────────────────────────────────

fn phase5_gc_after_deletes(
    engine: &StorageEngine,
    ctx: &RequestContext,
    total_stored: usize,
) -> Vec<(usize, usize)> {
    // Delete 10% of stored files, capped at 10K for time reasons
    let delete_target = (total_stored / 10).min(10_000);
    log_progress(&format!(
        "\n--- PHASE 5: Delete {} files + GC ---",
        delete_target
    ));

    let ops = DirectoryOps::new(engine);
    let mut rng = SimpleRng::new(999);

    // Generate unique random file indices to delete
    let mut to_delete: Vec<usize> = (0..total_stored).collect();
    rng.shuffle(&mut to_delete);
    to_delete.truncate(delete_target);

    let delete_start = Instant::now();
    let mut deleted = 0usize;
    let mut delete_errors = 0usize;
    for &flat in &to_delete {
        let shard = flat / FILES_PER_SHARD;
        let file = flat % FILES_PER_SHARD;
        let path = file_path(shard, file);
        match ops.delete_file(ctx, &path) {
            Ok(()) => deleted += 1,
            Err(_) => delete_errors += 1,
        }

        if (deleted + delete_errors).is_multiple_of(2_000) && (deleted + delete_errors) > 0 {
            log_progress(&format!(
                "  Deleted {} / {} ({:.1}s)",
                deleted + delete_errors,
                delete_target,
                delete_start.elapsed().as_secs_f64()
            ));
        }
    }
    let delete_elapsed = delete_start.elapsed();
    let del_rate = if delete_elapsed.as_secs_f64() > 0.0 {
        deleted as f64 / delete_elapsed.as_secs_f64()
    } else {
        0.0
    };
    log_progress(&format!(
        "  Delete phase: {} deleted, {} errors in {:.2}s ({:.0} deletes/s)",
        deleted, delete_errors, delete_elapsed.as_secs_f64(), del_rate
    ));

    let stats_before = engine.stats();
    log_progress(&format!(
        "  Before GC: {} voids, {} void bytes",
        stats_before.void_count, stats_before.void_space_bytes
    ));

    // Run GC
    let gc_start = Instant::now();
    let gc_result = run_gc(engine, ctx, false).expect("GC failed");
    let gc_elapsed = gc_start.elapsed();

    let stats_after = engine.stats();
    log_progress(&format!(
        "  GC: scanned={}, live={}, garbage={}, reclaimed={} bytes, {:.2}s",
        gc_result.versions_scanned,
        gc_result.live_entries,
        gc_result.garbage_entries,
        gc_result.reclaimed_bytes,
        gc_elapsed.as_secs_f64()
    ));
    log_progress(&format!(
        "  After GC: {} voids, {} void bytes, {:.1} MB on disk",
        stats_after.void_count,
        stats_after.void_space_bytes,
        stats_after.db_file_size_bytes as f64 / (1024.0 * 1024.0)
    ));
    print_separator();

    // Build surviving file list
    let deleted_set: std::collections::HashSet<usize> = to_delete.into_iter().collect();
    (0..total_stored)
        .filter(|i| !deleted_set.contains(i))
        .map(|flat| (flat / FILES_PER_SHARD, flat % FILES_PER_SHARD))
        .collect()
}

// ─── Phase 6: Verify Data Integrity ─────────────────────────────────────────

fn phase6_verify_integrity(engine: &StorageEngine, surviving: &[(usize, usize)]) {
    let count = VERIFY_COUNT.min(surviving.len());
    log_progress(&format!(
        "\n--- PHASE 6: Verify Integrity ({} random reads from {} survivors) ---",
        count,
        surviving.len()
    ));

    let ops = DirectoryOps::new(engine);
    let mut rng = SimpleRng::new(7777);

    let start = Instant::now();
    let mut ok = 0usize;
    let mut mismatch = 0usize;
    let mut not_found = 0usize;

    for _ in 0..count {
        let idx = rng.next_usize(surviving.len());
        let (shard, file) = surviving[idx];
        let path = file_path(shard, file);
        let expected = file_content(shard, file);

        match ops.read_file_buffered(&path) {
            Ok(data) => {
                if data == expected {
                    ok += 1;
                } else {
                    mismatch += 1;
                    if mismatch <= 3 {
                        eprintln!(
                            "  MISMATCH at {}: got {:?}, expected {:?}",
                            path,
                            String::from_utf8_lossy(&data),
                            String::from_utf8_lossy(&expected)
                        );
                    }
                }
            }
            Err(e) => {
                not_found += 1;
                if not_found <= 3 {
                    eprintln!("  NOT FOUND {}: {}", path, e);
                }
            }
        }
    }

    let elapsed = start.elapsed();
    log_progress(&format!(
        "  Verified {} files in {:.2}s: {} ok, {} mismatch, {} not_found",
        count,
        elapsed.as_secs_f64(),
        ok,
        mismatch,
        not_found
    ));

    if mismatch > 0 || not_found > 0 {
        log_progress("  WARNING: Data integrity issues detected!");
    } else {
        log_progress("  All integrity checks PASSED.");
    }
    print_separator();
}

// ─── Main ───────────────────────────────────────────────────────────────────

fn main() {
    let bench_dir = format!("/tmp/aeordb-bench-{}", std::process::id());
    std::fs::create_dir_all(&bench_dir).expect("create bench dir");

    let db_path = format!("{}/bench.aeordb", bench_dir);

    print_separator();
    log_progress("AeorDB Stress Benchmark");
    log_progress(&format!("Database: {}", db_path));
    log_progress(&format!(
        "Target: {} files across {} shards x {} files/shard",
        TARGET_FILES, SHARD_COUNT, FILES_PER_SHARD,
    ));
    print_separator();

    let engine = Arc::new(StorageEngine::create(&db_path).expect("create engine"));
    let ctx = RequestContext::system();

    // Ensure root directory exists
    {
        let ops = DirectoryOps::new(&engine);
        ops.ensure_root_directory(&ctx).expect("ensure root");
    }

    let overall_start = Instant::now();

    // Phase 1: Bulk store (10 min timeout)
    let total_stored = phase1_bulk_store(&engine, &ctx);

    if total_stored == 0 {
        log_progress("No files stored, aborting remaining phases.");
        let _ = std::fs::remove_dir_all(&bench_dir);
        return;
    }

    // Phase 2: Random reads
    phase2_random_reads(&engine, total_stored);

    // Phase 3: Query benchmark (with indexed subset)
    phase3_query_benchmark(&engine, &ctx);

    // Phase 4: Concurrent read/write
    phase4_concurrent(&engine, total_stored);

    // Phase 5: GC after deletes
    let surviving = phase5_gc_after_deletes(&engine, &ctx, total_stored);

    // Phase 6: Verify integrity
    phase6_verify_integrity(&engine, &surviving);

    let overall_elapsed = overall_start.elapsed();
    print_separator();
    log_progress(&format!("BENCHMARK COMPLETE in {:.1}s", overall_elapsed.as_secs_f64()));
    log_progress(&format!(
        "Final DB: {:.1} MB, {} files stored",
        engine.stats().db_file_size_bytes as f64 / (1024.0 * 1024.0),
        total_stored
    ));
    print_separator();

    // Cleanup
    drop(engine);
    if let Err(e) = std::fs::remove_dir_all(&bench_dir) {
        eprintln!("Warning: failed to clean up {}: {}", bench_dir, e);
    } else {
        log_progress(&format!("Cleaned up {}", bench_dir));
    }
    let _ = std::fs::remove_file("/tmp/aeordb-bench-progress.txt");
}
