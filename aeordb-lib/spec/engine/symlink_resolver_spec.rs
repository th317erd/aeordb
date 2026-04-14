use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::errors::EngineError;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::symlink_resolver::{resolve_symlink, ResolvedTarget, MAX_SYMLINK_DEPTH};
use aeordb::engine::RequestContext;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
    let ctx = RequestContext::system();
    let path = dir.path().join("test.aeor");
    let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();
    engine
}

fn store_file(engine: &StorageEngine, path: &str, content: &[u8]) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file(&ctx, path, content, None).unwrap();
}

fn store_symlink(engine: &StorageEngine, path: &str, target: &str) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_symlink(&ctx, path, target).unwrap();
}

// --- 1. Resolve symlink to file ---

#[test]
fn test_resolve_symlink_to_file() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_file(&engine, "/data.txt", b"hello");
    store_symlink(&engine, "/link", "/data.txt");

    match resolve_symlink(&engine, "/link") {
        Ok(ResolvedTarget::File(record)) => {
            assert_eq!(record.total_size, 5);
            assert_eq!(record.path, "/data.txt");
        }
        other => panic!("Expected File, got {:?}", other),
    }
}

// --- 2. Resolve symlink to directory ---

#[test]
fn test_resolve_symlink_to_directory() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    // Create directory /stuff by storing a file inside it
    store_file(&engine, "/stuff/inner.txt", b"x");
    store_symlink(&engine, "/shortcut", "/stuff");

    match resolve_symlink(&engine, "/shortcut") {
        Ok(ResolvedTarget::Directory(path)) => {
            assert_eq!(path, "/stuff");
        }
        other => panic!("Expected Directory, got {:?}", other),
    }
}

// --- 3. Resolve chain of 2 symlinks ---

#[test]
fn test_resolve_chain_2() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_file(&engine, "/file.txt", b"data!");
    store_symlink(&engine, "/link2", "/file.txt");
    store_symlink(&engine, "/link1", "/link2");

    match resolve_symlink(&engine, "/link1") {
        Ok(ResolvedTarget::File(record)) => {
            assert_eq!(record.total_size, 5);
            assert_eq!(record.path, "/file.txt");
        }
        other => panic!("Expected File, got {:?}", other),
    }
}

// --- 4. Resolve chain of 3 symlinks ---

#[test]
fn test_resolve_chain_3() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_file(&engine, "/file.txt", b"data!");
    store_symlink(&engine, "/c", "/file.txt");
    store_symlink(&engine, "/b", "/c");
    store_symlink(&engine, "/a", "/b");

    match resolve_symlink(&engine, "/a") {
        Ok(ResolvedTarget::File(record)) => {
            assert_eq!(record.total_size, 5);
            assert_eq!(record.path, "/file.txt");
        }
        other => panic!("Expected File, got {:?}", other),
    }
}

// --- 5. Dangling symlink ---

#[test]
fn test_resolve_dangling() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_symlink(&engine, "/link", "/nonexistent");

    match resolve_symlink(&engine, "/link") {
        Err(EngineError::NotFound(msg)) => {
            assert!(
                msg.contains("Dangling symlink"),
                "Error should mention 'Dangling symlink': {}",
                msg
            );
            assert!(
                msg.contains("/nonexistent"),
                "Error should mention the missing target: {}",
                msg
            );
        }
        other => panic!("Expected NotFound (dangling), got {:?}", other),
    }
}

// --- 6. Cycle detection: /a -> /b -> /a ---

#[test]
fn test_resolve_cycle_2() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_symlink(&engine, "/a", "/b");
    store_symlink(&engine, "/b", "/a");

    match resolve_symlink(&engine, "/a") {
        Err(EngineError::CyclicSymlink(msg)) => {
            assert!(msg.contains("/a"), "Cycle message should contain /a: {}", msg);
            assert!(msg.contains("/b"), "Cycle message should contain /b: {}", msg);
        }
        other => panic!("Expected CyclicSymlink, got {:?}", other),
    }
}

// --- 7. Self-reference: /a -> /a ---

#[test]
fn test_resolve_self_reference() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_symlink(&engine, "/a", "/a");

    match resolve_symlink(&engine, "/a") {
        Err(EngineError::CyclicSymlink(msg)) => {
            assert!(msg.contains("/a"), "Cycle message should contain /a: {}", msg);
        }
        other => panic!("Expected CyclicSymlink, got {:?}", other),
    }
}

// --- 8. Max depth exceeded ---

#[test]
fn test_resolve_max_depth() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    // Create file at the end of the chain
    store_file(&engine, "/chain_target", b"end");

    // Create MAX_SYMLINK_DEPTH + 1 symlinks: chain_0 -> chain_1 -> ... -> chain_32 -> chain_target
    // That's 33 hops to reach the file, exceeding the limit of 32.
    let count = MAX_SYMLINK_DEPTH + 1; // 33
    store_symlink(
        &engine,
        &format!("/chain_{}", count - 1),
        "/chain_target",
    );
    for i in (0..count - 1).rev() {
        store_symlink(
            &engine,
            &format!("/chain_{}", i),
            &format!("/chain_{}", i + 1),
        );
    }

    match resolve_symlink(&engine, "/chain_0") {
        Err(EngineError::SymlinkDepthExceeded(msg)) => {
            assert!(
                msg.contains(&format!("{}", MAX_SYMLINK_DEPTH)),
                "Message should contain the depth limit: {}",
                msg
            );
        }
        other => panic!("Expected SymlinkDepthExceeded, got {:?}", other),
    }
}

// --- 9. Dangling symlink becomes valid after target is created ---

#[test]
fn test_resolve_dangling_then_create_target() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_symlink(&engine, "/link", "/target.txt");

    // Should fail: dangling
    match resolve_symlink(&engine, "/link") {
        Err(EngineError::NotFound(msg)) => {
            assert!(msg.contains("Dangling symlink"), "Expected dangling: {}", msg);
        }
        other => panic!("Expected NotFound (dangling), got {:?}", other),
    }

    // Now create the target
    store_file(&engine, "/target.txt", b"now exists");

    // Should succeed
    match resolve_symlink(&engine, "/link") {
        Ok(ResolvedTarget::File(record)) => {
            assert_eq!(record.total_size, 10);
            assert_eq!(record.path, "/target.txt");
        }
        other => panic!("Expected File after creating target, got {:?}", other),
    }
}

// --- 10. Resolving a regular file (not a symlink) ---

#[test]
fn test_resolve_not_a_symlink_file() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_file(&engine, "/file.txt", b"regular");

    match resolve_symlink(&engine, "/file.txt") {
        Ok(ResolvedTarget::File(record)) => {
            assert_eq!(record.total_size, 7);
            assert_eq!(record.path, "/file.txt");
        }
        other => panic!("Expected File for regular file, got {:?}", other),
    }
}

// --- 11. Resolving a regular directory (not a symlink) ---

#[test]
fn test_resolve_not_a_symlink_directory() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_file(&engine, "/mydir/child.txt", b"x");

    match resolve_symlink(&engine, "/mydir") {
        Ok(ResolvedTarget::Directory(path)) => {
            assert_eq!(path, "/mydir");
        }
        other => panic!("Expected Directory for regular dir, got {:?}", other),
    }
}

// --- 12. Resolving a completely nonexistent path (not a symlink, not anything) ---

#[test]
fn test_resolve_nonexistent_path() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    match resolve_symlink(&engine, "/does_not_exist") {
        Err(EngineError::NotFound(msg)) => {
            assert!(
                msg.contains("Dangling symlink") || msg.contains("does not exist"),
                "Should indicate target doesn't exist: {}",
                msg
            );
        }
        other => panic!("Expected NotFound, got {:?}", other),
    }
}

// --- 13. Chain where intermediate hop is dangling ---

#[test]
fn test_resolve_chain_with_dangling_intermediate() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    store_symlink(&engine, "/a", "/b");
    store_symlink(&engine, "/b", "/c");
    // /c does not exist -- dangling at the end of a chain

    match resolve_symlink(&engine, "/a") {
        Err(EngineError::NotFound(msg)) => {
            assert!(
                msg.contains("/c"),
                "Should mention the dangling target /c: {}",
                msg
            );
        }
        other => panic!("Expected NotFound (dangling chain), got {:?}", other),
    }
}

// --- 14. Chain just under MAX_SYMLINK_DEPTH succeeds ---

#[test]
fn test_resolve_chain_at_max_depth_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    // Create a chain of MAX_SYMLINK_DEPTH - 1 symlinks -> file (31 hops).
    // After 31 hops depth=31, on the 32nd iteration depth=31 < 32,
    // and we find the file. This is the longest chain that succeeds.
    let chain_len = MAX_SYMLINK_DEPTH - 1; // 31
    store_file(&engine, "/deep_target", b"deep");

    store_symlink(
        &engine,
        &format!("/deep_{}", chain_len - 1),
        "/deep_target",
    );
    for i in (0..chain_len - 1).rev() {
        store_symlink(
            &engine,
            &format!("/deep_{}", i),
            &format!("/deep_{}", i + 1),
        );
    }

    match resolve_symlink(&engine, "/deep_0") {
        Ok(ResolvedTarget::File(record)) => {
            assert_eq!(record.total_size, 4);
            assert_eq!(record.path, "/deep_target");
        }
        other => panic!(
            "Expected File for chain at exactly MAX_SYMLINK_DEPTH, got {:?}",
            other
        ),
    }
}
