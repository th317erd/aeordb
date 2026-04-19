use std::collections::HashMap;
use std::sync::Arc;

use aeordb::engine::engine_counters::{EngineCounters, CountersSnapshot};
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine, VersionManager};

fn create_engine(directory: &tempfile::TempDir) -> StorageEngine {
    let ctx = RequestContext::system();
    let path = directory.path().join("test.aeor");
    let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();
    engine
}

// ─── 1. All counters start at zero ──────────────────────────────────────────

#[test]
fn test_new_counters_are_zero() {
    let counters = EngineCounters::new();
    let snapshot = counters.snapshot();

    assert_eq!(snapshot.files, 0);
    assert_eq!(snapshot.directories, 0);
    assert_eq!(snapshot.symlinks, 0);
    assert_eq!(snapshot.chunks, 0);
    assert_eq!(snapshot.snapshots, 0);
    assert_eq!(snapshot.forks, 0);
    assert_eq!(snapshot.logical_data_size, 0);
    assert_eq!(snapshot.chunk_data_size, 0);
    assert_eq!(snapshot.void_space, 0);
    assert_eq!(snapshot.writes_total, 0);
    assert_eq!(snapshot.reads_total, 0);
    assert_eq!(snapshot.bytes_written_total, 0);
    assert_eq!(snapshot.bytes_read_total, 0);
    assert_eq!(snapshot.chunks_deduped_total, 0);
    assert_eq!(snapshot.write_buffer_depth, 0);
}

// ─── 2. Increment / decrement produces correct count ────────────────────────

#[test]
fn test_increment_decrement() {
    let counters = EngineCounters::new();

    for _ in 0..5 {
        counters.increment_files();
    }
    for _ in 0..3 {
        counters.decrement_files();
    }

    let snapshot = counters.snapshot();
    assert_eq!(snapshot.files, 2);
}

#[test]
fn test_increment_decrement_directories() {
    let counters = EngineCounters::new();
    for _ in 0..7 {
        counters.increment_directories();
    }
    for _ in 0..4 {
        counters.decrement_directories();
    }
    assert_eq!(counters.snapshot().directories, 3);
}

#[test]
fn test_increment_decrement_symlinks() {
    let counters = EngineCounters::new();
    for _ in 0..6 {
        counters.increment_symlinks();
    }
    for _ in 0..2 {
        counters.decrement_symlinks();
    }
    assert_eq!(counters.snapshot().symlinks, 4);
}

#[test]
fn test_increment_decrement_chunks() {
    let counters = EngineCounters::new();
    for _ in 0..10 {
        counters.increment_chunks();
    }
    for _ in 0..6 {
        counters.decrement_chunks();
    }
    assert_eq!(counters.snapshot().chunks, 4);
}

#[test]
fn test_increment_decrement_snapshots() {
    let counters = EngineCounters::new();
    for _ in 0..3 {
        counters.increment_snapshots();
    }
    counters.decrement_snapshots();
    assert_eq!(counters.snapshot().snapshots, 2);
}

#[test]
fn test_increment_decrement_forks() {
    let counters = EngineCounters::new();
    for _ in 0..4 {
        counters.increment_forks();
    }
    for _ in 0..3 {
        counters.decrement_forks();
    }
    assert_eq!(counters.snapshot().forks, 1);
}

// ─── 3. Snapshot captures all fields ────────────────────────────────────────

#[test]
fn test_snapshot_captures_all_fields() {
    let counters = EngineCounters::new();

    counters.increment_files();
    counters.increment_files();
    counters.increment_directories();
    counters.increment_symlinks();
    counters.increment_chunks();
    counters.increment_chunks();
    counters.increment_chunks();
    counters.increment_snapshots();
    counters.increment_forks();
    counters.add_logical_data_size(1024);
    counters.add_chunk_data_size(512);
    counters.add_void_space(256);
    counters.increment_writes();
    counters.increment_writes();
    counters.increment_reads();
    counters.add_bytes_written(2048);
    counters.add_bytes_read(4096);
    counters.increment_chunks_deduped();
    counters.set_write_buffer_depth(42);

    let snapshot = counters.snapshot();

    assert_eq!(snapshot.files, 2);
    assert_eq!(snapshot.directories, 1);
    assert_eq!(snapshot.symlinks, 1);
    assert_eq!(snapshot.chunks, 3);
    assert_eq!(snapshot.snapshots, 1);
    assert_eq!(snapshot.forks, 1);
    assert_eq!(snapshot.logical_data_size, 1024);
    assert_eq!(snapshot.chunk_data_size, 512);
    assert_eq!(snapshot.void_space, 256);
    assert_eq!(snapshot.writes_total, 2);
    assert_eq!(snapshot.reads_total, 1);
    assert_eq!(snapshot.bytes_written_total, 2048);
    assert_eq!(snapshot.bytes_read_total, 4096);
    assert_eq!(snapshot.chunks_deduped_total, 1);
    assert_eq!(snapshot.write_buffer_depth, 42);
}

// ─── 4. Reconcile overwrites values ─────────────────────────────────────────

#[test]
fn test_reconcile_overwrites_values() {
    let counters = EngineCounters::new();

    // Set everything to 100
    for _ in 0..100 {
        counters.increment_files();
        counters.increment_directories();
        counters.increment_symlinks();
        counters.increment_chunks();
        counters.increment_snapshots();
        counters.increment_forks();
    }
    counters.add_logical_data_size(100);
    counters.add_chunk_data_size(100);
    counters.add_void_space(100);
    counters.set_write_buffer_depth(100);

    // Also set monotonic counters to verify they are NOT overwritten
    for _ in 0..100 {
        counters.increment_writes();
        counters.increment_reads();
    }
    counters.add_bytes_written(100);
    counters.add_bytes_read(100);

    // Reconcile with a snapshot that has all count/size fields at 50
    let reconcile_snapshot = CountersSnapshot {
        files: 50,
        directories: 50,
        symlinks: 50,
        chunks: 50,
        snapshots: 50,
        forks: 50,
        logical_data_size: 50,
        chunk_data_size: 50,
        void_space: 50,
        writes_total: 999,        // should NOT overwrite
        reads_total: 999,         // should NOT overwrite
        bytes_written_total: 999, // should NOT overwrite
        bytes_read_total: 999,    // should NOT overwrite
        chunks_deduped_total: 999, // should NOT overwrite
        write_buffer_depth: 50,
    };

    counters.reconcile(&reconcile_snapshot);

    let result = counters.snapshot();

    // Count and size fields should be reconciled to 50
    assert_eq!(result.files, 50);
    assert_eq!(result.directories, 50);
    assert_eq!(result.symlinks, 50);
    assert_eq!(result.chunks, 50);
    assert_eq!(result.snapshots, 50);
    assert_eq!(result.forks, 50);
    assert_eq!(result.logical_data_size, 50);
    assert_eq!(result.chunk_data_size, 50);
    assert_eq!(result.void_space, 50);
    assert_eq!(result.write_buffer_depth, 50);

    // Monotonic counters should NOT have been overwritten
    assert_eq!(result.writes_total, 100);
    assert_eq!(result.reads_total, 100);
    assert_eq!(result.bytes_written_total, 100);
    assert_eq!(result.bytes_read_total, 100);
}

// ─── 5. Concurrent increments ──────────────────────────────────────────────

#[test]
fn test_concurrent_increments() {
    let counters = Arc::new(EngineCounters::new());
    let mut handles = Vec::new();

    for _ in 0..10 {
        let counters_clone = Arc::clone(&counters);
        handles.push(std::thread::spawn(move || {
            for _ in 0..1000 {
                counters_clone.increment_files();
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let snapshot = counters.snapshot();
    assert_eq!(snapshot.files, 10_000);
}

#[test]
fn test_concurrent_mixed_increments_and_decrements() {
    let counters = Arc::new(EngineCounters::new());

    // Pre-load 5000 so decrements don't saturate to zero
    for _ in 0..5000 {
        counters.increment_chunks();
    }

    let mut handles = Vec::new();

    // 5 threads increment 1000 times each = +5000
    for _ in 0..5 {
        let counters_clone = Arc::clone(&counters);
        handles.push(std::thread::spawn(move || {
            for _ in 0..1000 {
                counters_clone.increment_chunks();
            }
        }));
    }

    // 5 threads decrement 1000 times each = -5000
    for _ in 0..5 {
        let counters_clone = Arc::clone(&counters);
        handles.push(std::thread::spawn(move || {
            for _ in 0..1000 {
                counters_clone.decrement_chunks();
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let snapshot = counters.snapshot();
    assert_eq!(snapshot.chunks, 5000);
}

#[test]
fn test_concurrent_size_adds() {
    let counters = Arc::new(EngineCounters::new());
    let mut handles = Vec::new();

    for _ in 0..10 {
        let counters_clone = Arc::clone(&counters);
        handles.push(std::thread::spawn(move || {
            for _ in 0..1000 {
                counters_clone.add_logical_data_size(100);
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let snapshot = counters.snapshot();
    assert_eq!(snapshot.logical_data_size, 1_000_000);
}

// ─── 6. Monotonic counters never decrement ──────────────────────────────────

#[test]
fn test_monotonic_counters_never_decrement() {
    let counters = EngineCounters::new();

    // Writes and reads only go up -- they use fetch_add, no sub method exists.
    for _ in 0..10 {
        counters.increment_writes();
        counters.increment_reads();
    }
    counters.add_bytes_written(500);
    counters.add_bytes_read(300);
    counters.increment_chunks_deduped();
    counters.increment_chunks_deduped();
    counters.increment_chunks_deduped();

    let snapshot = counters.snapshot();
    assert_eq!(snapshot.writes_total, 10);
    assert_eq!(snapshot.reads_total, 10);
    assert_eq!(snapshot.bytes_written_total, 500);
    assert_eq!(snapshot.bytes_read_total, 300);
    assert_eq!(snapshot.chunks_deduped_total, 3);

    // More increments only increase
    for _ in 0..5 {
        counters.increment_writes();
        counters.increment_reads();
    }
    counters.add_bytes_written(100);
    counters.add_bytes_read(100);

    let snapshot_after = counters.snapshot();
    assert_eq!(snapshot_after.writes_total, 15);
    assert_eq!(snapshot_after.reads_total, 15);
    assert_eq!(snapshot_after.bytes_written_total, 600);
    assert_eq!(snapshot_after.bytes_read_total, 400);
}

// ─── 7. Size counters add and subtract correctly ────────────────────────────

#[test]
fn test_size_counters_add_and_subtract() {
    let counters = EngineCounters::new();

    counters.add_logical_data_size(100);
    counters.sub_logical_data_size(30);
    assert_eq!(counters.snapshot().logical_data_size, 70);

    counters.add_chunk_data_size(200);
    counters.sub_chunk_data_size(50);
    assert_eq!(counters.snapshot().chunk_data_size, 150);

    counters.add_void_space(500);
    counters.sub_void_space(100);
    assert_eq!(counters.snapshot().void_space, 400);
}

#[test]
fn test_size_counters_saturate_at_zero() {
    let counters = EngineCounters::new();

    // Subtracting more than current value should saturate to 0, not wrap
    counters.add_logical_data_size(10);
    counters.sub_logical_data_size(100);
    assert_eq!(counters.snapshot().logical_data_size, 0);

    counters.add_chunk_data_size(5);
    counters.sub_chunk_data_size(999);
    assert_eq!(counters.snapshot().chunk_data_size, 0);

    counters.add_void_space(1);
    counters.sub_void_space(u64::MAX);
    assert_eq!(counters.snapshot().void_space, 0);
}

#[test]
fn test_count_saturates_at_zero() {
    let counters = EngineCounters::new();

    // Decrementing from zero should stay at zero
    counters.decrement_files();
    assert_eq!(counters.snapshot().files, 0);

    counters.increment_files();
    counters.decrement_files();
    counters.decrement_files();
    assert_eq!(counters.snapshot().files, 0);
}

// ─── 8. Initialize from KV ─────────────────────────────────────────────────

#[test]
fn test_initialize_from_kv_counts_files() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = DirectoryOps::new(&engine);

    // Store 5 files
    ops.store_file(&ctx, "/alpha.txt", b"hello alpha", None).unwrap();
    ops.store_file(&ctx, "/beta.txt", b"hello beta", None).unwrap();
    ops.store_file(&ctx, "/gamma.txt", b"hello gamma", None).unwrap();
    ops.store_file(&ctx, "/delta.txt", b"hello delta", None).unwrap();
    ops.store_file(&ctx, "/epsilon.txt", b"hello epsilon", None).unwrap();

    // Cross-check: counters should match the existing stats() method
    let stats = engine.stats();

    let counters = EngineCounters::initialize_from_kv(&engine);
    let snapshot = counters.snapshot();

    // Each store_file creates 3 KV entries with KV_TYPE_FILE_RECORD
    // (content-addressed, identity, and path-based), so 5 files = 15 entries
    assert_eq!(snapshot.files, stats.file_count as u64, "should match stats().file_count");
    assert_eq!(snapshot.directories, stats.directory_count as u64, "should match stats().directory_count");
    assert_eq!(snapshot.chunks, stats.chunk_count as u64, "should match stats().chunk_count");
    assert!(snapshot.logical_data_size > 0, "files have nonzero total_size");
    assert!(snapshot.chunk_data_size > 0, "chunks have nonzero data");
}

#[test]
fn test_initialize_from_kv_counts_all_types() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = DirectoryOps::new(&engine);
    let version_manager = VersionManager::new(&engine);

    // Create directories via file storage (parent directories created automatically)
    ops.store_file(&ctx, "/docs/readme.txt", b"readme", None).unwrap();
    ops.store_file(&ctx, "/docs/changelog.txt", b"changelog", None).unwrap();

    // Create a symlink
    ops.store_symlink(&ctx, "/link", "/docs/readme.txt").unwrap();

    // Create a snapshot
    version_manager.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();

    // Create a fork
    version_manager.create_fork(&ctx, "feature-branch", None).unwrap();

    let stats = engine.stats();

    let counters = EngineCounters::initialize_from_kv(&engine);
    let snapshot = counters.snapshot();

    // Verify counters match stats() for all shared fields
    assert_eq!(snapshot.files, stats.file_count as u64, "should match stats().file_count");
    assert_eq!(snapshot.directories, stats.directory_count as u64, "should match stats().directory_count");
    assert_eq!(snapshot.snapshots, stats.snapshot_count as u64, "should match stats().snapshot_count");
    assert_eq!(snapshot.forks, stats.fork_count as u64, "should match stats().fork_count");
    assert!(snapshot.chunks > 0, "files produce chunks");

    // Additionally verify types we know were created
    assert!(snapshot.files >= 6, "at least 6 file record KV entries (2 files x 3 entries each)");
    assert!(snapshot.symlinks >= 1, "should count at least 1 symlink");
    assert!(snapshot.snapshots >= 1, "should count at least 1 snapshot");
    assert!(snapshot.forks >= 1, "should count at least 1 fork");
}

#[test]
fn test_initialize_from_kv_sums_logical_size() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = DirectoryOps::new(&engine);

    let data_a = vec![0u8; 100];
    let data_b = vec![1u8; 200];
    let data_c = vec![2u8; 300];

    ops.store_file(&ctx, "/a.bin", &data_a, None).unwrap();
    ops.store_file(&ctx, "/b.bin", &data_b, None).unwrap();
    ops.store_file(&ctx, "/c.bin", &data_c, None).unwrap();

    let counters = EngineCounters::initialize_from_kv(&engine);
    let snapshot = counters.snapshot();

    // Each store_file creates 3 KV entries with KV_TYPE_FILE_RECORD, each
    // deserializing to the same total_size, so logical_data_size =
    // 3 * (100 + 200 + 300) = 1800.
    assert_eq!(snapshot.logical_data_size, 1800);
}

#[test]
fn test_initialize_from_kv_empty_database() {
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);

    let counters = EngineCounters::initialize_from_kv(&engine);
    let snapshot = counters.snapshot();

    // A freshly created engine with root directory has at least 1 directory
    assert!(snapshot.directories >= 1, "should count root directory");
    assert_eq!(snapshot.files, 0);
    assert_eq!(snapshot.symlinks, 0);
    assert_eq!(snapshot.snapshots, 0);
    assert_eq!(snapshot.forks, 0);
    assert_eq!(snapshot.logical_data_size, 0);

    // Throughput counters should be zero -- startup scan doesn't count I/O
    assert_eq!(snapshot.writes_total, 0);
    assert_eq!(snapshot.reads_total, 0);
}

// ─── Edge cases and additional coverage ─────────────────────────────────────

#[test]
fn test_set_write_buffer_depth_overwrites() {
    let counters = EngineCounters::new();

    counters.set_write_buffer_depth(10);
    assert_eq!(counters.snapshot().write_buffer_depth, 10);

    counters.set_write_buffer_depth(0);
    assert_eq!(counters.snapshot().write_buffer_depth, 0);

    counters.set_write_buffer_depth(999);
    assert_eq!(counters.snapshot().write_buffer_depth, 999);
}

#[test]
fn test_reconcile_does_not_affect_monotonic_counters() {
    let counters = EngineCounters::new();

    counters.increment_writes();
    counters.increment_reads();
    counters.add_bytes_written(42);
    counters.add_bytes_read(84);
    counters.increment_chunks_deduped();

    // Reconcile with a snapshot that has different monotonic values
    let reconcile_snapshot = CountersSnapshot {
        files: 0,
        directories: 0,
        symlinks: 0,
        chunks: 0,
        snapshots: 0,
        forks: 0,
        logical_data_size: 0,
        chunk_data_size: 0,
        void_space: 0,
        writes_total: 0,
        reads_total: 0,
        bytes_written_total: 0,
        bytes_read_total: 0,
        chunks_deduped_total: 0,
        write_buffer_depth: 0,
    };

    counters.reconcile(&reconcile_snapshot);

    let result = counters.snapshot();
    assert_eq!(result.writes_total, 1, "writes_total should be unchanged");
    assert_eq!(result.reads_total, 1, "reads_total should be unchanged");
    assert_eq!(result.bytes_written_total, 42, "bytes_written_total should be unchanged");
    assert_eq!(result.bytes_read_total, 84, "bytes_read_total should be unchanged");
    assert_eq!(result.chunks_deduped_total, 1, "chunks_deduped_total should be unchanged");
}

#[test]
fn test_snapshot_is_serializable() {
    let counters = EngineCounters::new();
    counters.increment_files();
    counters.add_logical_data_size(42);

    let snapshot = counters.snapshot();
    let json = serde_json::to_string(&snapshot).unwrap();

    assert!(json.contains("\"files\":1"));
    assert!(json.contains("\"logical_data_size\":42"));
}

#[test]
fn test_multiple_snapshots_are_independent() {
    let counters = EngineCounters::new();

    counters.increment_files();
    let snapshot_1 = counters.snapshot();

    counters.increment_files();
    counters.increment_files();
    let snapshot_2 = counters.snapshot();

    // Snapshots should be independent -- snapshot_1 should still show 1
    assert_eq!(snapshot_1.files, 1);
    assert_eq!(snapshot_2.files, 3);
}

#[test]
fn test_large_size_values() {
    let counters = EngineCounters::new();

    // Test with multi-gigabyte values
    let one_gb: u64 = 1_073_741_824;
    counters.add_logical_data_size(one_gb * 100);
    counters.add_chunk_data_size(one_gb * 50);

    let snapshot = counters.snapshot();
    assert_eq!(snapshot.logical_data_size, one_gb * 100);
    assert_eq!(snapshot.chunk_data_size, one_gb * 50);
}
