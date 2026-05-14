use std::sync::Arc;
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};

fn test_engine() -> (Arc<StorageEngine>, tempfile::TempDir) {
    aeordb::server::create_temp_engine_for_tests()
}

#[test]
fn create_single_file_no_sidecars() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();

    let _engine = StorageEngine::create(db_str).unwrap();

    // Only the .aeordb file and .lock should exist
    let files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    assert!(files.contains(&"test.aeordb".to_string()), "DB file should exist");
    assert!(!files.iter().any(|f| f.ends_with(".kv")), "No .kv sidecar should exist");
    assert!(!files.iter().any(|f| f.contains("hot")), "No hot sidecar should exist");
}

#[test]
fn store_and_retrieve_100_entries() {
    let (engine, _dir) = test_engine();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    for i in 0..100 {
        let path = format!("/test/file_{}.txt", i);
        let data = format!("content {}", i);
        ops.store_file_buffered(&ctx, &path, data.as_bytes(), Some("text/plain")).unwrap();
    }

    for i in 0..100 {
        let path = format!("/test/file_{}.txt", i);
        let data = ops.read_file_buffered(&path).unwrap();
        assert_eq!(String::from_utf8_lossy(&data), format!("content {}", i));
    }
}

#[test]
fn reopen_preserves_data() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();

    // Session 1: write data
    {
        let engine = { let e = Arc::new(StorageEngine::create(db_str).unwrap()); let ops = DirectoryOps::new(&e); let ctx = RequestContext::system(); ops.ensure_root_directory(&ctx).unwrap(); e };
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();

        ops.store_file_buffered(&ctx, "/persistent.txt", b"hello world", Some("text/plain")).unwrap();
        ops.store_file_buffered(&ctx, "/dir/nested.txt", b"nested data", Some("text/plain")).unwrap();
        engine.shutdown().unwrap();
    }

    // Session 2: data should be readable
    {
        let engine = Arc::new(StorageEngine::open(db_str).unwrap());
        let ops = DirectoryOps::new(&engine);

        let data = ops.read_file_buffered("/persistent.txt").unwrap();
        assert_eq!(data, b"hello world");

        let data = ops.read_file_buffered("/dir/nested.txt").unwrap();
        assert_eq!(data, b"nested data");
    }
}

#[test]
fn reopen_with_many_entries() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();

    // Session 1: write 500 files
    {
        let engine = { let e = Arc::new(StorageEngine::create(db_str).unwrap()); let ops = DirectoryOps::new(&e); let ctx = RequestContext::system(); ops.ensure_root_directory(&ctx).unwrap(); e };
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();

        for i in 0..500 {
            let path = format!("/batch/file_{}.txt", i);
            ops.store_file_buffered(&ctx, &path, format!("data_{}", i).as_bytes(), None).unwrap();
        }
        engine.shutdown().unwrap();
    }

    // Session 2: all 500 should be readable
    {
        let engine = Arc::new(StorageEngine::open(db_str).unwrap());
        let ops = DirectoryOps::new(&engine);

        for i in 0..500 {
            let path = format!("/batch/file_{}.txt", i);
            let data = ops.read_file_buffered(&path).unwrap();
            assert_eq!(String::from_utf8_lossy(&data), format!("data_{}", i));
        }
    }
}

#[test]
fn hot_tail_survives_no_flush() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();

    // Session 1: write data, do NOT call shutdown (simulates abrupt close)
    {
        let engine = { let e = Arc::new(StorageEngine::create(db_str).unwrap()); let ops = DirectoryOps::new(&e); let ctx = RequestContext::system(); ops.ensure_root_directory(&ctx).unwrap(); e };
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();

        ops.store_file_buffered(&ctx, "/hot.txt", b"hot data", Some("text/plain")).unwrap();
        // Drop without shutdown — hot buffer may not be flushed to KV pages
    }

    // Session 2: data should still be found (from hot tail or WAL scan)
    {
        let engine = Arc::new(StorageEngine::open(db_str).unwrap());
        let ops = DirectoryOps::new(&engine);

        let data = ops.read_file_buffered("/hot.txt").unwrap();
        assert_eq!(data, b"hot data");
    }
}

#[test]
fn delete_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();

    // Session 1: create, then delete
    {
        let engine = { let e = Arc::new(StorageEngine::create(db_str).unwrap()); let ops = DirectoryOps::new(&e); let ctx = RequestContext::system(); ops.ensure_root_directory(&ctx).unwrap(); e };
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();

        ops.store_file_buffered(&ctx, "/ephemeral.txt", b"gone soon", None).unwrap();
        ops.delete_file(&ctx, "/ephemeral.txt").unwrap();
        ops.store_file_buffered(&ctx, "/survivor.txt", b"still here", None).unwrap();
        engine.shutdown().unwrap();
    }

    // Session 2
    {
        let engine = Arc::new(StorageEngine::open(db_str).unwrap());
        let ops = DirectoryOps::new(&engine);

        assert!(ops.read_file_buffered("/ephemeral.txt").is_err(), "Deleted file should stay deleted");
        assert_eq!(ops.read_file_buffered("/survivor.txt").unwrap(), b"still here");
    }
}

#[test]
fn no_sidecar_files_during_operations() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();

    let engine = { let e = Arc::new(StorageEngine::create(db_str).unwrap()); let ops = DirectoryOps::new(&e); let ctx = RequestContext::system(); ops.ensure_root_directory(&ctx).unwrap(); e };
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    // Store many files to trigger flushes
    for i in 0..600 {
        ops.store_file_buffered(&ctx, &format!("/files/f{}.txt", i), b"x", None).unwrap();
    }

    // Check: no sidecar files
    let files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    let sidecars: Vec<_> = files.iter()
        .filter(|f| f.ends_with(".kv") || f.contains("hot"))
        .collect();
    assert!(sidecars.is_empty(), "No sidecar files should exist, found: {:?}", sidecars);
}
