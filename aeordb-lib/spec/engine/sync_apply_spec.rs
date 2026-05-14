use aeordb::engine::file_record::FileRecord;
use aeordb::engine::merge::MergeOp;
use aeordb::engine::symlink_record::SymlinkRecord;
use aeordb::engine::sync_apply::apply_merge_operations;
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};
use aeordb::server::create_temp_engine_for_tests;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Store a file and return its FileRecord (retrieved from the engine).
fn store_and_get_record(
    engine: &StorageEngine,
    path: &str,
    data: &[u8],
) -> (Vec<u8>, FileRecord) {
    let context = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file_buffered(&context, path, data, Some("text/plain")).unwrap();

    // Walk the tree to find the record
    let head = engine.head_hash().unwrap();
    let tree = aeordb::engine::tree_walker::walk_version_tree(engine, &head).unwrap();
    tree.files.get(path).expect("file should exist after store").clone()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_apply_adds_file() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let context = RequestContext::system();

    // First, store a file so we have valid chunks in the engine
    let (file_hash, file_record) = store_and_get_record(&engine, "/source.txt", b"hello world");

    // Delete the original file so we can test adding via merge
    let ops = DirectoryOps::new(&engine);
    ops.delete_file(&context, "/source.txt").unwrap();

    // Now apply a merge operation to add a file using those chunks
    let operations = vec![MergeOp::AddFile {
        path: "/merged.txt".to_string(),
        file_hash,
        file_record,
    }];

    apply_merge_operations(&engine, &context, &operations).unwrap();

    // Verify the file exists
    let head = engine.head_hash().unwrap();
    let tree = aeordb::engine::tree_walker::walk_version_tree(&engine, &head).unwrap();
    assert!(tree.files.contains_key("/merged.txt"), "merged file should exist");
}

#[test]
fn test_apply_deletes_file() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let context = RequestContext::system();

    // Store a file first
    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&context, "/to_delete.txt", b"data", Some("text/plain")).unwrap();

    // Verify it exists
    let head = engine.head_hash().unwrap();
    let tree = aeordb::engine::tree_walker::walk_version_tree(&engine, &head).unwrap();
    assert!(tree.files.contains_key("/to_delete.txt"));

    // Apply merge delete operation
    let operations = vec![MergeOp::DeleteFile {
        path: "/to_delete.txt".to_string(),
    }];

    apply_merge_operations(&engine, &context, &operations).unwrap();

    // Verify file is gone
    let head = engine.head_hash().unwrap();
    let tree = aeordb::engine::tree_walker::walk_version_tree(&engine, &head).unwrap();
    assert!(
        !tree.files.contains_key("/to_delete.txt"),
        "file should be deleted"
    );
}

#[test]
fn test_apply_delete_nonexistent_file_succeeds() {
    // Deleting a file that doesn't exist should NOT error
    let (engine, _temp) = create_temp_engine_for_tests();
    let context = RequestContext::system();

    let operations = vec![MergeOp::DeleteFile {
        path: "/does_not_exist.txt".to_string(),
    }];

    let result = apply_merge_operations(&engine, &context, &operations);
    assert!(result.is_ok(), "deleting nonexistent file should not fail");
}

#[test]
fn test_apply_adds_symlink() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let context = RequestContext::system();

    let symlink_record = SymlinkRecord {
        path: "/link".to_string(),
        target: "/some/target".to_string(),
        created_at: 1000,
        updated_at: 1000,
    };

    let operations = vec![MergeOp::AddSymlink {
        path: "/link".to_string(),
        symlink_hash: vec![1, 2, 3],
        symlink_record,
    }];

    apply_merge_operations(&engine, &context, &operations).unwrap();

    // Verify symlink exists
    let head = engine.head_hash().unwrap();
    let tree = aeordb::engine::tree_walker::walk_version_tree(&engine, &head).unwrap();
    assert!(tree.symlinks.contains_key("/link"), "symlink should exist");
    assert_eq!(tree.symlinks["/link"].1.target, "/some/target");
}

#[test]
fn test_apply_deletes_symlink() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let context = RequestContext::system();

    // Store a symlink first
    let ops = DirectoryOps::new(&engine);
    ops.store_symlink(&context, "/link", "/target").unwrap();

    // Verify it exists
    let head = engine.head_hash().unwrap();
    let tree = aeordb::engine::tree_walker::walk_version_tree(&engine, &head).unwrap();
    assert!(tree.symlinks.contains_key("/link"));

    // Apply merge delete
    let operations = vec![MergeOp::DeleteSymlink {
        path: "/link".to_string(),
    }];

    apply_merge_operations(&engine, &context, &operations).unwrap();

    // Verify symlink is gone
    let head = engine.head_hash().unwrap();
    let tree = aeordb::engine::tree_walker::walk_version_tree(&engine, &head).unwrap();
    assert!(
        !tree.symlinks.contains_key("/link"),
        "symlink should be deleted"
    );
}

#[test]
fn test_apply_missing_chunk_fails() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let context = RequestContext::system();

    // Create a FileRecord referencing a chunk that does not exist
    let fake_chunk_hash = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x11, 0x22, 0x33,
                               0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB,
                               0xCC, 0xDD, 0xEE, 0xFF, 0x01, 0x02, 0x03, 0x04,
                               0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C];

    let file_record = FileRecord {
        path: "/broken.txt".to_string(),
        content_type: Some("text/plain".to_string()),
        total_size: 100,
        created_at: 1000,
        updated_at: 1000,
        metadata: Vec::new(),
        chunk_hashes: vec![fake_chunk_hash],
    };

    let operations = vec![MergeOp::AddFile {
        path: "/broken.txt".to_string(),
        file_hash: vec![1, 2, 3],
        file_record,
    }];

    let result = apply_merge_operations(&engine, &context, &operations);
    assert!(result.is_err(), "should fail when chunk is missing");

    let error_message = format!("{}", result.unwrap_err());
    assert!(
        error_message.contains("Missing chunk"),
        "error should mention missing chunk, got: {}",
        error_message,
    );
}

#[test]
fn test_apply_multiple_operations_atomically() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let context = RequestContext::system();

    // Store two files first
    let (file_hash_a, file_record_a) = store_and_get_record(&engine, "/a.txt", b"content a");
    let (_file_hash_b, _file_record_b) = store_and_get_record(&engine, "/b.txt", b"content b");

    let ops = DirectoryOps::new(&engine);
    ops.store_symlink(&context, "/old_link", "/nowhere").unwrap();

    // Apply multiple operations: add a file, delete a file, add a symlink, delete a symlink
    let operations = vec![
        MergeOp::AddFile {
            path: "/new_from_a.txt".to_string(),
            file_hash: file_hash_a,
            file_record: file_record_a,
        },
        MergeOp::AddSymlink {
            path: "/new_link".to_string(),
            symlink_hash: vec![1],
            symlink_record: SymlinkRecord {
                path: "/new_link".to_string(),
                target: "/new_from_a.txt".to_string(),
                created_at: 1000,
                updated_at: 1000,
            },
        },
        MergeOp::DeleteFile {
            path: "/b.txt".to_string(),
        },
        MergeOp::DeleteSymlink {
            path: "/old_link".to_string(),
        },
    ];

    apply_merge_operations(&engine, &context, &operations).unwrap();

    let head = engine.head_hash().unwrap();
    let tree = aeordb::engine::tree_walker::walk_version_tree(&engine, &head).unwrap();

    assert!(tree.files.contains_key("/new_from_a.txt"), "new file from a should exist");
    assert!(!tree.files.contains_key("/b.txt"), "b.txt should be deleted");
    assert!(tree.symlinks.contains_key("/new_link"), "new symlink should exist");
    assert!(!tree.symlinks.contains_key("/old_link"), "old symlink should be deleted");
}

#[test]
fn test_apply_delete_symlink_nonexistent_succeeds() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let context = RequestContext::system();

    let operations = vec![MergeOp::DeleteSymlink {
        path: "/ghost_link".to_string(),
    }];

    let result = apply_merge_operations(&engine, &context, &operations);
    assert!(result.is_ok(), "deleting nonexistent symlink should not fail");
}

#[test]
fn test_apply_empty_operations() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let context = RequestContext::system();

    let result = apply_merge_operations(&engine, &context, &[]);
    assert!(result.is_ok(), "empty operations should succeed");
}
