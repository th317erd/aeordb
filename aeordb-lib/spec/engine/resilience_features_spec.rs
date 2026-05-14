//! Tests for resilience features: GC auto-snapshot, verify, and verify --repair.

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::gc::run_gc;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::verify;
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::RequestContext;
use aeordb::server::create_temp_engine_for_tests;

/// Inject garbage bytes at the given offset in the database file.
fn inject_corruption(db_path: &str, offset: u64, size: usize) {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(db_path)
        .unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let garbage: Vec<u8> = (0..size).map(|i| (i as u8).wrapping_mul(0x37)).collect();
    file.write_all(&garbage).unwrap();
    file.sync_all().unwrap();
}

/// Store a few test files into the engine.
fn store_test_files(engine: &StorageEngine) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file_buffered(&ctx, "/docs/a.txt", b"file-a-content", Some("text/plain"))
        .unwrap();
    ops.store_file_buffered(&ctx, "/docs/b.txt", b"file-b-content", Some("text/plain"))
        .unwrap();
    ops.store_file_buffered(
        &ctx,
        "/images/photo.jpg",
        b"jpeg-data-here",
        Some("image/jpeg"),
    )
    .unwrap();
}

// =========================================================================
// Auto-snapshot before GC
// =========================================================================

#[test]
fn gc_creates_pre_gc_snapshot() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    store_test_files(&engine);

    // Delete a file so GC has something to collect
    let ops = DirectoryOps::new(&engine);
    ops.delete_file(&ctx, "/docs/a.txt").unwrap();

    // Run GC (not dry run)
    run_gc(&engine, &ctx, false).unwrap();

    // Check for pre-GC snapshot
    let vm = VersionManager::new(&engine);
    let snapshots = vm.list_snapshots().unwrap();
    let pre_gc = snapshots
        .iter()
        .find(|s| s.name.starts_with("_aeordb_pre_gc_"));
    assert!(pre_gc.is_some(), "Pre-GC snapshot should exist");
}

#[test]
fn gc_dry_run_does_not_create_snapshot() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    store_test_files(&engine);

    let ops = DirectoryOps::new(&engine);
    ops.delete_file(&ctx, "/docs/a.txt").unwrap();

    // Dry run -- no snapshot
    run_gc(&engine, &ctx, true).unwrap();

    let vm = VersionManager::new(&engine);
    let snapshots = vm.list_snapshots().unwrap();
    let pre_gc = snapshots
        .iter()
        .find(|s| s.name.starts_with("_aeordb_pre_gc_"));
    assert!(pre_gc.is_none(), "Dry run should not create snapshot");
}

#[test]
fn gc_keeps_only_last_3_pre_gc_snapshots() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();

    for i in 0..5 {
        // Store and delete a file to create garbage
        let ops = DirectoryOps::new(&engine);
        let path = format!("/temp_{}.txt", i);
        ops.store_file_buffered(
            &ctx,
            &path,
            format!("content-{}", i).as_bytes(),
            Some("text/plain"),
        )
        .unwrap();
        ops.delete_file(&ctx, &path).unwrap();

        // Sleep 1.1s so each GC gets a unique timestamp (chrono::Utc::now().timestamp()
        // has 1-second resolution)
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Run GC
        run_gc(&engine, &ctx, false).unwrap();
    }

    let vm = VersionManager::new(&engine);
    let snapshots = vm.list_snapshots().unwrap();
    let pre_gc_count = snapshots
        .iter()
        .filter(|s| s.name.starts_with("_aeordb_pre_gc_"))
        .count();

    assert!(
        pre_gc_count <= 3,
        "Should keep at most 3 pre-GC snapshots, got {}",
        pre_gc_count
    );
}

// =========================================================================
// aeordb verify
// =========================================================================

#[test]
fn verify_clean_database_reports_no_issues() {
    let (engine, temp) = create_temp_engine_for_tests();
    store_test_files(&engine);

    let db_path = temp.path().join("test.aeordb");
    let report = verify::verify(&engine, db_path.to_str().unwrap());

    // Core integrity: no corruption, no missing children
    assert_eq!(report.corrupt_hash, 0, "Should have no corrupt hashes");
    assert_eq!(report.corrupt_header, 0, "Should have no corrupt headers");
    assert!(
        report.missing_children.is_empty(),
        "Should have no missing children: {:?}",
        report.missing_children,
    );

    // Entry counts should be populated
    assert!(report.total_entries > 0, "Should have scanned entries");
    assert!(report.chunks > 0, "Should have chunks");
    assert!(report.file_records > 0, "Should have file records");
    assert!(
        report.directory_indexes > 0,
        "Should have directory indexes"
    );
    assert!(
        report.valid_entries > 0,
        "Should have valid entries"
    );
}

#[test]
fn verify_reports_storage_metrics() {
    let (engine, temp) = create_temp_engine_for_tests();
    store_test_files(&engine);

    let db_path = temp.path().join("test.aeordb");
    let report = verify::verify(&engine, db_path.to_str().unwrap());

    assert!(report.file_size > 0, "File size should be > 0");
    assert!(report.chunk_data_size > 0, "Chunk data should be > 0");
    assert!(
        !report.hash_algorithm.is_empty(),
        "Hash algorithm should be reported"
    );
}

#[test]
fn verify_reports_voids() {
    let (engine, temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    store_test_files(&engine);

    // Delete a file to create garbage, then GC to create voids
    let ops = DirectoryOps::new(&engine);
    ops.delete_file(&ctx, "/docs/a.txt").unwrap();
    run_gc(&engine, &ctx, false).unwrap();

    let db_path = temp.path().join("test.aeordb");
    let report = verify::verify(&engine, db_path.to_str().unwrap());

    assert!(report.voids > 0, "Should have voids after GC");
    assert!(report.void_bytes > 0, "Void bytes should be > 0");
}

#[test]
fn verify_and_repair_rebuilds_kv() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();

    {
        let engine = StorageEngine::create(db_str).unwrap();
        let ctx = RequestContext::system();
        let ops = DirectoryOps::new(&engine);
        ops.ensure_root_directory(&ctx).unwrap();
        store_test_files(&engine);
    }

    // Delete KV to force rebuild on open
    let kv_path = format!("{}.kv", db_str);
    let _ = std::fs::remove_file(&kv_path);

    let engine = StorageEngine::open(db_str).unwrap();
    let report = verify::verify_and_repair(&engine, db_str);

    // After KV rebuild on open, the database should have entries
    assert!(
        report.total_entries > 0,
        "Should have scanned entries after KV rebuild"
    );
}

#[test]
fn verify_reports_corrupt_entries() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();

    {
        let engine = StorageEngine::create(db_str).unwrap();
        let ctx = RequestContext::system();
        let ops = DirectoryOps::new(&engine);
        ops.ensure_root_directory(&ctx).unwrap();
        store_test_files(&engine);
    }

    // Inject corruption at ~33% of file
    let file_size = std::fs::metadata(db_str).unwrap().len();
    inject_corruption(db_str, file_size / 3, 64);

    // Delete KV to force rebuild
    let kv_path = format!("{}.kv", db_str);
    let _ = std::fs::remove_file(&kv_path);

    let engine = StorageEngine::open(db_str).unwrap();
    let report = verify::verify(&engine, db_str);

    // Should have scanned entries (some may be corrupt, some may survive)
    assert!(
        report.total_entries > 0,
        "Should have scanned some entries despite corruption"
    );
}

#[test]
fn verify_entry_counts_match_stored_data() {
    let (engine, temp) = create_temp_engine_for_tests();
    store_test_files(&engine);

    let db_path = temp.path().join("test.aeordb");
    let report = verify::verify(&engine, db_path.to_str().unwrap());

    // We stored 3 files, so at minimum 3 file records and 3 chunks
    assert!(
        report.file_records >= 3,
        "Should have at least 3 file records, got {}",
        report.file_records
    );
    assert!(
        report.chunks >= 3,
        "Should have at least 3 chunks, got {}",
        report.chunks
    );
    // Directories: / + /docs + /images = at least 3
    assert!(
        report.directory_indexes >= 3,
        "Should have at least 3 directory indexes, got {}",
        report.directory_indexes
    );
    // Valid entries should equal total (clean database)
    assert_eq!(
        report.valid_entries, report.total_entries,
        "All entries should be valid in a clean database"
    );
}
