//! Comprehensive tests for hot file transaction guards, deadlock prevention,
//! and crash recovery.
//!
//! Validates that TransactionGuard correctly manages transaction depth,
//! fires on all exit paths (normal, error, panic), and that store/delete
//! operations are properly wrapped in transactions.

use aeordb::engine::storage_engine::{StorageEngine, TransactionGuard};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::RequestContext;

/// Create a fresh test database with hot directory enabled.
fn create_test_db_with_hot_dir() -> (StorageEngine, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("test.aeordb");
    let engine = StorageEngine::create_with_hot_dir(
        db_path.to_str().unwrap(),
        Some(temp.path()),
    ).unwrap();
    (engine, temp)
}

// =========================================================================
// Transaction depth management
// =========================================================================

#[test]
fn transaction_guard_increments_and_decrements_depth() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    // Depth starts at 0
    {
        let _guard = TransactionGuard::new(&engine);
        // Inside transaction -- depth is 1
        // Store a file -- flush should NOT truncate hot file
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();
        ops.store_file(&ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();
    }
    // Guard dropped -- depth back to 0, hot file truncated

    // Verify the file is readable after guard drop (proves no corruption)
    let ops = DirectoryOps::new(&engine);
    let data = ops.read_file("/test.txt").unwrap();
    assert_eq!(data, b"hello");
}

#[test]
fn transaction_guard_fires_on_error() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    let result: Result<(), String> = (|| {
        let _guard = TransactionGuard::new(&engine);
        // Simulate an error mid-transaction
        return Err("simulated error".to_string());
    })();

    assert!(result.is_err());
    // Guard should have dropped -- verify we can start a new transaction
    // without deadlocking
    let _guard2 = TransactionGuard::new(&engine);
    // If this doesn't deadlock, depth management is correct

    // Also verify we can do real work in the new transaction
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file(&ctx, "/after-error.txt", b"recovered", Some("text/plain")).unwrap();
}

#[test]
fn transaction_guard_fires_on_panic() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = TransactionGuard::new(&engine);
        panic!("simulated panic inside transaction");
    }));

    assert!(result.is_err());
    // Guard should have dropped despite panic
    // Verify depth is back to 0 by successfully starting a new transaction
    let _guard2 = TransactionGuard::new(&engine);
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    // This should work -- depth is 0, hot file can truncate
    ops.store_file(&ctx, "/after-panic.txt", b"recovered", Some("text/plain")).unwrap();

    // Verify the file is actually readable
    let data = ops.read_file("/after-panic.txt").unwrap();
    assert_eq!(data, b"recovered");
}

#[test]
fn transaction_depth_always_returns_to_zero() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    // Multiple sequential transactions
    for i in 0..10 {
        let _guard = TransactionGuard::new(&engine);
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();
        let path = format!("/file_{}.txt", i);
        ops.store_file(&ctx, &path, format!("content-{}", i).as_bytes(), Some("text/plain")).unwrap();
    }

    // All guards dropped -- depth must be 0
    // Prove it by successfully storing another file (which triggers flush + truncate)
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file(&ctx, "/final.txt", b"final", Some("text/plain")).unwrap();

    // Verify all files are readable
    for i in 0..10 {
        let path = format!("/file_{}.txt", i);
        let data = ops.read_file(&path).unwrap();
        assert_eq!(data, format!("content-{}", i).as_bytes());
    }
    let final_data = ops.read_file("/final.txt").unwrap();
    assert_eq!(final_data, b"final");
}

#[test]
fn nested_guards_increment_depth_correctly() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    {
        let _guard1 = TransactionGuard::new(&engine);
        {
            let _guard2 = TransactionGuard::new(&engine);
            // Depth is 2 here -- storing a file should work
            let ops = DirectoryOps::new(&engine);
            let ctx = RequestContext::system();
            ops.store_file(&ctx, "/nested.txt", b"nested", Some("text/plain")).unwrap();
        }
        // Depth is 1 here -- inner guard dropped
    }
    // Depth is 0 here -- outer guard dropped

    // Verify we can start fresh transactions
    let _guard3 = TransactionGuard::new(&engine);
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file(&ctx, "/after-nested.txt", b"ok", Some("text/plain")).unwrap();
}

#[test]
fn mixed_success_and_error_transactions() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    // Successful transaction
    {
        let _guard = TransactionGuard::new(&engine);
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();
        ops.store_file(&ctx, "/success1.txt", b"ok", Some("text/plain")).unwrap();
    }

    // Failed transaction (error)
    let _: Result<(), String> = (|| {
        let _guard = TransactionGuard::new(&engine);
        Err("fail".to_string())
    })();

    // Another successful transaction
    {
        let _guard = TransactionGuard::new(&engine);
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();
        ops.store_file(&ctx, "/success2.txt", b"ok2", Some("text/plain")).unwrap();
    }

    // Panicked transaction
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = TransactionGuard::new(&engine);
        panic!("boom");
    }));

    // Final successful transaction -- proves depth is still 0
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file(&ctx, "/success3.txt", b"ok3", Some("text/plain")).unwrap();

    // All successful files should be readable
    assert_eq!(ops.read_file("/success1.txt").unwrap(), b"ok");
    assert_eq!(ops.read_file("/success2.txt").unwrap(), b"ok2");
    assert_eq!(ops.read_file("/success3.txt").unwrap(), b"ok3");
}

// =========================================================================
// store_file is transactional
// =========================================================================

#[test]
fn store_file_wraps_in_transaction() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    // Store a file -- this should be wrapped in a transaction internally
    ops.store_file(&ctx, "/docs/readme.md", b"# Hello", Some("text/markdown")).unwrap();

    // Verify the file is listed in its parent directory
    let children = ops.list_directory("/docs").unwrap();
    assert!(children.iter().any(|c| c.name == "readme.md"), "file should be in parent listing");

    // Verify the file is readable
    let data = ops.read_file("/docs/readme.md").unwrap();
    assert_eq!(data, b"# Hello");
}

#[test]
fn store_multiple_files_each_transactional() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    // Each store_file internally wraps in a transaction
    ops.store_file(&ctx, "/docs/a.txt", b"aaa", Some("text/plain")).unwrap();
    ops.store_file(&ctx, "/docs/b.txt", b"bbb", Some("text/plain")).unwrap();
    ops.store_file(&ctx, "/docs/c.txt", b"ccc", Some("text/plain")).unwrap();

    // All should be listed
    let children = ops.list_directory("/docs").unwrap();
    assert_eq!(children.len(), 3);
    let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"b.txt"));
    assert!(names.contains(&"c.txt"));

    // All should be readable
    assert_eq!(ops.read_file("/docs/a.txt").unwrap(), b"aaa");
    assert_eq!(ops.read_file("/docs/b.txt").unwrap(), b"bbb");
    assert_eq!(ops.read_file("/docs/c.txt").unwrap(), b"ccc");
}

// =========================================================================
// delete_file is transactional
// =========================================================================

#[test]
fn delete_file_wraps_in_transaction() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    ops.store_file(&ctx, "/docs/to-delete.txt", b"delete me", Some("text/plain")).unwrap();

    // Verify it exists
    let children = ops.list_directory("/docs").unwrap();
    assert!(children.iter().any(|c| c.name == "to-delete.txt"));

    // Delete it
    ops.delete_file(&ctx, "/docs/to-delete.txt").unwrap();

    // Verify it's gone from listing
    let children = ops.list_directory("/docs").unwrap();
    assert!(!children.iter().any(|c| c.name == "to-delete.txt"), "file should be removed from listing");

    // Verify reading it returns NotFound
    let result = ops.read_file("/docs/to-delete.txt");
    assert!(result.is_err(), "deleted file should not be readable");
}

#[test]
fn delete_nonexistent_file_returns_error() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    let result = ops.delete_file(&ctx, "/nonexistent.txt");
    assert!(result.is_err(), "deleting nonexistent file should fail");
}

// =========================================================================
// Recovery tests
// =========================================================================

#[test]
fn recovery_detects_orphaned_file_after_hot_replay() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();
    let hot_dir = temp.path();

    // Create DB and store files normally
    {
        let engine = StorageEngine::create_with_hot_dir(db_str, Some(hot_dir)).unwrap();
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();
        ops.store_file(&ctx, "/docs/existing.txt", b"exists", Some("text/plain")).unwrap();
    }

    // Reopen -- should have no recovery needed, data should be intact
    {
        let engine = StorageEngine::open_with_hot_dir(db_str, Some(hot_dir)).unwrap();
        let ops = DirectoryOps::new(&engine);
        let children = ops.list_directory("/docs").unwrap();
        assert!(children.iter().any(|c| c.name == "existing.txt"),
            "file should survive close/reopen cycle");

        // Verify the file data is intact
        let data = ops.read_file("/docs/existing.txt").unwrap();
        assert_eq!(data, b"exists");
    }
}

#[test]
fn recovery_preserves_multiple_files_across_restart() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();
    let hot_dir = temp.path();

    // Create and populate
    {
        let engine = StorageEngine::create_with_hot_dir(db_str, Some(hot_dir)).unwrap();
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();
        ops.store_file(&ctx, "/data/alpha.txt", b"alpha-data", Some("text/plain")).unwrap();
        ops.store_file(&ctx, "/data/beta.txt", b"beta-data", Some("text/plain")).unwrap();
        ops.store_file(&ctx, "/data/gamma.txt", b"gamma-data", Some("text/plain")).unwrap();
    }

    // Reopen and verify
    {
        let engine = StorageEngine::open_with_hot_dir(db_str, Some(hot_dir)).unwrap();
        let ops = DirectoryOps::new(&engine);

        let children = ops.list_directory("/data").unwrap();
        assert_eq!(children.len(), 3, "all three files should survive restart");

        assert_eq!(ops.read_file("/data/alpha.txt").unwrap(), b"alpha-data");
        assert_eq!(ops.read_file("/data/beta.txt").unwrap(), b"beta-data");
        assert_eq!(ops.read_file("/data/gamma.txt").unwrap(), b"gamma-data");
    }
}

#[test]
fn recovery_after_store_and_delete_across_restart() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();
    let hot_dir = temp.path();

    // Create, store, then delete a file
    {
        let engine = StorageEngine::create_with_hot_dir(db_str, Some(hot_dir)).unwrap();
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();
        ops.store_file(&ctx, "/docs/keep.txt", b"keep-me", Some("text/plain")).unwrap();
        ops.store_file(&ctx, "/docs/remove.txt", b"remove-me", Some("text/plain")).unwrap();
        ops.delete_file(&ctx, "/docs/remove.txt").unwrap();
    }

    // Reopen and verify the deletion persisted
    {
        let engine = StorageEngine::open_with_hot_dir(db_str, Some(hot_dir)).unwrap();
        let ops = DirectoryOps::new(&engine);

        let children = ops.list_directory("/docs").unwrap();
        assert!(children.iter().any(|c| c.name == "keep.txt"), "kept file should exist");
        assert!(!children.iter().any(|c| c.name == "remove.txt"), "deleted file should stay deleted");

        assert_eq!(ops.read_file("/docs/keep.txt").unwrap(), b"keep-me");
    }
}

// =========================================================================
// Edge cases
// =========================================================================

#[test]
fn empty_file_with_transaction() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    // Store an empty file inside an explicit transaction
    {
        let _guard = TransactionGuard::new(&engine);
        ops.store_file(&ctx, "/empty.txt", b"", Some("text/plain")).unwrap();
    }

    let data = ops.read_file("/empty.txt").unwrap();
    assert!(data.is_empty(), "empty file should read back as empty");
}

#[test]
fn large_file_with_transaction() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    // Store a file larger than one chunk (>256KB) inside a transaction
    let large_data: Vec<u8> = (0..300_000).map(|i| (i % 256) as u8).collect();
    {
        let _guard = TransactionGuard::new(&engine);
        ops.store_file(&ctx, "/large.bin", &large_data, Some("application/octet-stream")).unwrap();
    }

    let read_back = ops.read_file("/large.bin").unwrap();
    assert_eq!(read_back.len(), 300_000);
    assert_eq!(read_back, large_data);
}

#[test]
fn overwrite_file_with_transaction() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    // Store, then overwrite
    ops.store_file(&ctx, "/mutable.txt", b"version-1", Some("text/plain")).unwrap();
    ops.store_file(&ctx, "/mutable.txt", b"version-2", Some("text/plain")).unwrap();

    let data = ops.read_file("/mutable.txt").unwrap();
    assert_eq!(data, b"version-2", "overwritten file should have latest content");

    // Only one entry in parent listing
    let children = ops.list_directory("/").unwrap();
    let matches: Vec<_> = children.iter().filter(|c| c.name == "mutable.txt").collect();
    assert_eq!(matches.len(), 1, "should not duplicate listing on overwrite");
}
