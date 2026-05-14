use aeordb::engine::StorageEngine;

#[test]
fn second_open_of_same_database_is_rejected() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();

    // First open succeeds
    let engine1 = StorageEngine::create(db_str).expect("first create should succeed");

    // Second open of the same file should fail with a lock error
    let result = StorageEngine::open_with_hot_dir(db_str, None);
    assert!(result.is_err(), "second open should fail due to file lock");
    let err_msg = match result {
        Err(e) => format!("{}", e),
        Ok(_) => panic!("expected error"),
    };
    assert!(
        err_msg.contains("locked by another process"),
        "error should mention file lock, got: {}",
        err_msg,
    );

    // After dropping the first engine, a new open should succeed
    drop(engine1);
    let _engine2 = StorageEngine::open_with_hot_dir(db_str, None)
        .expect("open should succeed after first engine is dropped");
}

#[test]
fn second_create_of_same_path_is_rejected_while_locked() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test2.aeordb");
    let db_str = db_path.to_str().unwrap();

    let _engine1 = StorageEngine::create(db_str).expect("first create should succeed");

    // Trying to create again at the same path should fail (lock held)
    let result = StorageEngine::create(db_str);
    assert!(result.is_err(), "second create should fail due to file lock");
}

#[test]
fn lock_released_on_drop() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test3.aeordb");
    let db_str = db_path.to_str().unwrap();

    // Create, use, drop
    {
        let engine = StorageEngine::create(db_str).expect("create should succeed");
        // Store something to verify the DB works
        let ops = aeordb::engine::DirectoryOps::new(&engine);
        let ctx = aeordb::engine::RequestContext::system();
        ops.store_file_buffered(&ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();
    }
    // Engine dropped here — lock released

    // Re-open should succeed and data should be intact
    let engine2 = StorageEngine::open_with_hot_dir(db_str, None)
        .expect("open after drop should succeed");
    let ops = aeordb::engine::DirectoryOps::new(&engine2);
    let data = ops.read_file_buffered("/test.txt").expect("file should be readable");
    assert_eq!(data, b"hello");
}

#[test]
fn lock_file_does_not_interfere_with_different_databases() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db1_path = temp_dir.path().join("db1.aeordb");
    let db2_path = temp_dir.path().join("db2.aeordb");

    // Two different databases should both open fine
    let _engine1 = StorageEngine::create(db1_path.to_str().unwrap())
        .expect("first DB should open");
    let _engine2 = StorageEngine::create(db2_path.to_str().unwrap())
        .expect("second DB should open (different file)");
}
