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

// ─── Phase 2: Live counter instrumentation tests ─────────────────────────────

#[test]
fn test_live_counters_store_files() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = DirectoryOps::new(&engine);

    // Store 5 files
    ops.store_file(&ctx, "/a.txt", b"aaa", None).unwrap();
    ops.store_file(&ctx, "/b.txt", b"bbb", None).unwrap();
    ops.store_file(&ctx, "/c.txt", b"ccc", None).unwrap();
    ops.store_file(&ctx, "/d.txt", b"ddd", None).unwrap();
    ops.store_file(&ctx, "/e.txt", b"eee", None).unwrap();

    let snap = engine.counters().snapshot();
    // Each file stored produces one increment_files call
    assert_eq!(snap.files, 5, "should count 5 files");
    assert_eq!(snap.writes_total, 5, "should count 5 writes");
    assert_eq!(snap.bytes_written_total, 15, "should count 15 bytes written (3 * 5)");
    assert_eq!(snap.logical_data_size, 15, "logical data size should be 15");
}

#[test]
fn test_live_counters_delete_files() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = DirectoryOps::new(&engine);

    ops.store_file(&ctx, "/a.txt", b"aaa", None).unwrap();
    ops.store_file(&ctx, "/b.txt", b"bbb", None).unwrap();
    ops.store_file(&ctx, "/c.txt", b"ccc", None).unwrap();
    ops.store_file(&ctx, "/d.txt", b"ddd", None).unwrap();
    ops.store_file(&ctx, "/e.txt", b"eee", None).unwrap();

    ops.delete_file(&ctx, "/a.txt").unwrap();
    ops.delete_file(&ctx, "/b.txt").unwrap();

    let snap = engine.counters().snapshot();
    assert_eq!(snap.files, 3, "should have 3 files after deleting 2");
    assert_eq!(snap.logical_data_size, 9, "logical data size should be 9 after deleting 6 bytes");
}

#[test]
fn test_live_counters_symlink() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = DirectoryOps::new(&engine);

    ops.store_file(&ctx, "/target.txt", b"hello", None).unwrap();
    ops.store_symlink(&ctx, "/link", "/target.txt").unwrap();

    let snap = engine.counters().snapshot();
    assert_eq!(snap.symlinks, 1, "should count 1 symlink");
}

#[test]
fn test_live_counters_symlink_delete() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = DirectoryOps::new(&engine);

    ops.store_file(&ctx, "/target.txt", b"hello", None).unwrap();
    ops.store_symlink(&ctx, "/link1", "/target.txt").unwrap();
    ops.store_symlink(&ctx, "/link2", "/target.txt").unwrap();

    let snap = engine.counters().snapshot();
    assert_eq!(snap.symlinks, 2, "should count 2 symlinks");

    ops.delete_symlink(&ctx, "/link1").unwrap();

    let snap = engine.counters().snapshot();
    assert_eq!(snap.symlinks, 1, "should count 1 symlink after deleting 1");
}

#[test]
fn test_live_counters_snapshot() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let version_manager = VersionManager::new(&engine);

    version_manager.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();

    let snap = engine.counters().snapshot();
    assert_eq!(snap.snapshots, 1, "should count 1 snapshot");
}

#[test]
fn test_live_counters_snapshot_delete() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = aeordb::engine::DirectoryOps::new(&engine);
    let version_manager = VersionManager::new(&engine);

    // Writes between snapshots prevent dedup (back-to-back snapshots at the
    // same HEAD return the prior snapshot rather than creating a new one).
    ops.store_file(&ctx, "/a.txt", b"a", None).unwrap();
    version_manager.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();
    ops.store_file(&ctx, "/b.txt", b"b", None).unwrap();
    version_manager.create_snapshot(&ctx, "v2", HashMap::new()).unwrap();

    let snap = engine.counters().snapshot();
    assert_eq!(snap.snapshots, 2, "should count 2 snapshots");

    version_manager.delete_snapshot(&ctx, "v1").unwrap();

    let snap = engine.counters().snapshot();
    assert_eq!(snap.snapshots, 1, "should count 1 snapshot after deleting 1");
}

#[test]
fn test_live_counters_fork() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let version_manager = VersionManager::new(&engine);

    version_manager.create_fork(&ctx, "feature-a", None).unwrap();

    let snap = engine.counters().snapshot();
    assert_eq!(snap.forks, 1, "should count 1 fork");
}

#[test]
fn test_live_counters_fork_abandon() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let version_manager = VersionManager::new(&engine);

    version_manager.create_fork(&ctx, "feature-a", None).unwrap();
    version_manager.create_fork(&ctx, "feature-b", None).unwrap();

    let snap = engine.counters().snapshot();
    assert_eq!(snap.forks, 2, "should count 2 forks");

    version_manager.abandon_fork(&ctx, "feature-a").unwrap();

    let snap = engine.counters().snapshot();
    assert_eq!(snap.forks, 1, "should count 1 fork after abandoning 1");
}

#[test]
fn test_live_counters_reads() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = DirectoryOps::new(&engine);

    ops.store_file(&ctx, "/file.txt", b"hello world", None).unwrap();

    let snap_before = engine.counters().snapshot();
    assert_eq!(snap_before.reads_total, 0);

    let _data = ops.read_file("/file.txt").unwrap();
    let _data2 = ops.read_file("/file.txt").unwrap();

    let snap_after = engine.counters().snapshot();
    assert_eq!(snap_after.reads_total, 2, "should count 2 reads");
    assert_eq!(snap_after.bytes_read_total, 22, "should count 22 bytes read (11 * 2)");
}

#[test]
fn test_live_counters_overwrite_adjusts_logical_size() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = DirectoryOps::new(&engine);

    ops.store_file(&ctx, "/file.txt", b"short", None).unwrap();

    let snap1 = engine.counters().snapshot();
    assert_eq!(snap1.logical_data_size, 5, "initial logical size is 5");
    assert_eq!(snap1.files, 1, "should count 1 file");

    // Overwrite with longer content
    ops.store_file(&ctx, "/file.txt", b"much longer content", None).unwrap();

    let snap2 = engine.counters().snapshot();
    assert_eq!(snap2.files, 1, "overwrite should NOT increment file count");
    assert_eq!(snap2.logical_data_size, 19, "logical size should reflect new content");
}

#[test]
fn test_live_counters_overwrite_shrink_adjusts_logical_size() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = DirectoryOps::new(&engine);

    ops.store_file(&ctx, "/file.txt", b"this is a long piece of content", None).unwrap();

    let snap1 = engine.counters().snapshot();
    assert_eq!(snap1.logical_data_size, 31);

    // Overwrite with shorter content
    ops.store_file(&ctx, "/file.txt", b"tiny", None).unwrap();

    let snap2 = engine.counters().snapshot();
    assert_eq!(snap2.logical_data_size, 4, "logical size should shrink on overwrite");
}

#[test]
fn test_live_counters_chunk_dedup() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = DirectoryOps::new(&engine);

    let data = b"identical content";
    ops.store_file(&ctx, "/a.txt", data, None).unwrap();

    let snap1 = engine.counters().snapshot();
    let chunks_after_first = snap1.chunks;
    assert!(chunks_after_first >= 1, "at least 1 chunk stored");
    assert_eq!(snap1.chunks_deduped_total, 0, "no dedup on first store");

    // Store same content under different path — chunk should be deduped
    ops.store_file(&ctx, "/b.txt", data, None).unwrap();

    let snap2 = engine.counters().snapshot();
    assert_eq!(snap2.chunks, chunks_after_first, "chunk count should not increase on dedup");
    assert!(snap2.chunks_deduped_total >= 1, "should record at least 1 dedup hit");
}

#[test]
fn test_live_counters_symlink_update_does_not_double_count() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = DirectoryOps::new(&engine);

    ops.store_file(&ctx, "/target1.txt", b"hello", None).unwrap();
    ops.store_file(&ctx, "/target2.txt", b"world", None).unwrap();

    ops.store_symlink(&ctx, "/link", "/target1.txt").unwrap();
    let snap1 = engine.counters().snapshot();
    assert_eq!(snap1.symlinks, 1);

    // Update same symlink to point to a different target
    ops.store_symlink(&ctx, "/link", "/target2.txt").unwrap();
    let snap2 = engine.counters().snapshot();
    assert_eq!(snap2.symlinks, 1, "updating a symlink should NOT increment count");
}

#[test]
fn test_live_counters_full_lifecycle() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = DirectoryOps::new(&engine);
    let vm = VersionManager::new(&engine);

    // Store files
    ops.store_file(&ctx, "/doc1.txt", b"alpha", None).unwrap();
    ops.store_file(&ctx, "/doc2.txt", b"beta", None).unwrap();
    ops.store_file(&ctx, "/doc3.txt", b"gamma", None).unwrap();

    // Create symlink
    ops.store_symlink(&ctx, "/link1", "/doc1.txt").unwrap();

    // Create snapshot
    vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    // Create fork
    vm.create_fork(&ctx, "branch1", None).unwrap();

    let snap = engine.counters().snapshot();
    assert_eq!(snap.files, 3);
    assert_eq!(snap.symlinks, 1);
    assert_eq!(snap.snapshots, 1);
    assert_eq!(snap.forks, 1);
    assert_eq!(snap.writes_total, 3);
    assert_eq!(snap.logical_data_size, 14); // "alpha"(5) + "beta"(4) + "gamma"(5)

    // Delete one file and the fork
    ops.delete_file(&ctx, "/doc2.txt").unwrap();
    vm.abandon_fork(&ctx, "branch1").unwrap();

    let snap2 = engine.counters().snapshot();
    assert_eq!(snap2.files, 2);
    assert_eq!(snap2.forks, 0);
    assert_eq!(snap2.logical_data_size, 10); // "alpha"(5) + "gamma"(5)
    assert_eq!(snap2.snapshots, 1);
    assert_eq!(snap2.symlinks, 1);
}

#[test]
fn test_live_counters_initialized_on_create() {
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);

    // A freshly created engine should have initialized counters.
    // Note: directories counter starts at 0 on first create because
    // initialize_from_kv runs before ensure_root_directory is called
    // by the test helper. Directory operations (store_file, etc.) do
    // NOT increment the directory counter — only GC reconciliation or
    // engine re-open re-scans. This is by design: directory entries
    // are structural, not user-created objects worth tracking individually
    // in the hot path.
    let snap = engine.counters().snapshot();
    assert_eq!(snap.files, 0);
    assert_eq!(snap.symlinks, 0);
    assert_eq!(snap.snapshots, 0);
    assert_eq!(snap.forks, 0);
}

#[test]
fn test_live_counters_initialized_on_reopen() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("test.aeor");
    let path_str = path.to_str().unwrap();

    // Create and populate
    {
        let engine = StorageEngine::create(path_str).unwrap();
        let ops = DirectoryOps::new(&engine);
        ops.ensure_root_directory(&ctx).unwrap();
        ops.store_file(&ctx, "/a.txt", b"aaa", None).unwrap();
        ops.store_file(&ctx, "/b.txt", b"bbb", None).unwrap();
        ops.store_symlink(&ctx, "/link", "/a.txt").unwrap();
    }

    // Reopen — counters should be re-initialized from KV scan
    let engine = StorageEngine::open(path_str).unwrap();
    let snap = engine.counters().snapshot();

    // File records have 3 KV entries each (content, identity, path)
    // so files count = 6 (2 files x 3 entries)
    assert!(snap.files >= 2, "should count at least 2 file records");
    assert!(snap.symlinks >= 1, "should count at least 1 symlink");
    assert!(snap.directories >= 1, "should count root directory");
    assert!(snap.logical_data_size > 0, "should have nonzero logical data size");
}

#[test]
fn test_live_counters_empty_file_store() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = DirectoryOps::new(&engine);

    // Store an empty file — 0 bytes, 0 chunks
    ops.store_file(&ctx, "/empty.txt", b"", None).unwrap();

    let snap = engine.counters().snapshot();
    assert_eq!(snap.files, 1, "empty file should still count as a file");
    assert_eq!(snap.logical_data_size, 0, "empty file has 0 logical size");
    assert_eq!(snap.writes_total, 1, "should count 1 write");
    assert_eq!(snap.bytes_written_total, 0, "0 bytes written for empty file");
}

#[test]
fn test_live_counters_delete_nonexistent_file_does_not_decrement() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = DirectoryOps::new(&engine);

    ops.store_file(&ctx, "/exists.txt", b"data", None).unwrap();

    let snap_before = engine.counters().snapshot();
    assert_eq!(snap_before.files, 1);

    // Attempt to delete a file that doesn't exist — should error, not change counters
    let result = ops.delete_file(&ctx, "/nope.txt");
    assert!(result.is_err(), "deleting nonexistent file should error");

    let snap_after = engine.counters().snapshot();
    assert_eq!(snap_after.files, 1, "failed delete should not change file count");
}

#[test]
fn test_live_counters_delete_nonexistent_symlink_does_not_decrement() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = DirectoryOps::new(&engine);

    ops.store_file(&ctx, "/target.txt", b"data", None).unwrap();
    ops.store_symlink(&ctx, "/link", "/target.txt").unwrap();

    let snap_before = engine.counters().snapshot();
    assert_eq!(snap_before.symlinks, 1);

    let result = ops.delete_symlink(&ctx, "/nonexistent_link");
    assert!(result.is_err(), "deleting nonexistent symlink should error");

    let snap_after = engine.counters().snapshot();
    assert_eq!(snap_after.symlinks, 1, "failed delete should not change symlink count");
}

#[test]
fn test_live_counters_multiple_reads_accumulate() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let ops = DirectoryOps::new(&engine);

    ops.store_file(&ctx, "/file.txt", b"abcdef", None).unwrap();

    for _ in 0..10 {
        let _data = ops.read_file("/file.txt").unwrap();
    }

    let snap = engine.counters().snapshot();
    assert_eq!(snap.reads_total, 10, "should count 10 reads");
    assert_eq!(snap.bytes_read_total, 60, "should count 60 bytes read (6 * 10)");
}

#[test]
fn test_live_counters_fork_promote_decrements() {
    let ctx = RequestContext::system();
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    let vm = VersionManager::new(&engine);

    vm.create_fork(&ctx, "to-promote", None).unwrap();
    assert_eq!(engine.counters().snapshot().forks, 1);

    // Promote calls abandon_fork internally, which should decrement
    vm.promote_fork(&ctx, "to-promote").unwrap();
    assert_eq!(engine.counters().snapshot().forks, 0, "promoted fork should be decremented");
}
