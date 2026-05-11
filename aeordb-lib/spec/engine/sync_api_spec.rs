use aeordb::engine::conflict_store;
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::merge::{ConflictEntry, ConflictType, ConflictVersion};
use aeordb::engine::sync_api::{
    apply_sync_chunks, compute_sync_diff, get_needed_chunks, list_conflicts_typed,
};
use aeordb::engine::{RequestContext, StorageEngine};
use aeordb::server::create_temp_engine_for_tests;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn store_file(engine: &StorageEngine, path: &str, data: &[u8]) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file(&ctx, path, data, Some("text/plain"))
        .unwrap();
}

fn store_symlink(engine: &StorageEngine, path: &str, target: &str) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_symlink(&ctx, path, target).unwrap();
}

fn read_file(engine: &StorageEngine, path: &str) -> Vec<u8> {
    let ops = DirectoryOps::new(engine);
    ops.read_file(path).unwrap()
}

fn head_hash(engine: &StorageEngine) -> Vec<u8> {
    engine.head_hash().unwrap()
}

// ---------------------------------------------------------------------------
// 1. test_compute_diff_full
// ---------------------------------------------------------------------------

#[test]
fn test_compute_diff_full() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_file(&engine, "/docs/a.txt", b"alpha");
    store_file(&engine, "/docs/b.txt", b"bravo");

    let diff = compute_sync_diff(&engine, None, None, true).unwrap();

    // All files should be in files_added
    assert!(
        diff.files_added.len() >= 2,
        "expected at least 2 added files, got {}",
        diff.files_added.len()
    );
    let added_paths: Vec<&str> = diff.files_added.iter().map(|f| f.path.as_str()).collect();
    assert!(added_paths.contains(&"/docs/a.txt"));
    assert!(added_paths.contains(&"/docs/b.txt"));

    // No modified or deleted
    assert!(diff.files_modified.is_empty());
    assert!(diff.files_deleted.is_empty());

    // root_hash should be non-empty
    assert!(!diff.root_hash.is_empty());

    // chunk_hashes_needed should have hashes
    assert!(!diff.chunk_hashes_needed.is_empty());
}

// ---------------------------------------------------------------------------
// 2. test_compute_diff_incremental
// ---------------------------------------------------------------------------

#[test]
fn test_compute_diff_incremental() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_file(&engine, "/docs/a.txt", b"alpha");

    let snapshot_hash = head_hash(&engine);

    store_file(&engine, "/docs/b.txt", b"bravo");

    let diff = compute_sync_diff(&engine, Some(&snapshot_hash), None, true).unwrap();

    // Only /docs/b.txt should be added
    let added_paths: Vec<&str> = diff.files_added.iter().map(|f| f.path.as_str()).collect();
    assert!(
        added_paths.contains(&"/docs/b.txt"),
        "expected /docs/b.txt in added, got {:?}",
        added_paths
    );
    // /docs/a.txt should NOT be in added
    assert!(
        !added_paths.contains(&"/docs/a.txt"),
        "/docs/a.txt should not be in added"
    );
}

// ---------------------------------------------------------------------------
// 3. test_compute_diff_with_paths_filter
// ---------------------------------------------------------------------------

#[test]
fn test_compute_diff_with_paths_filter() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_file(&engine, "/a/x.txt", b"x data");
    store_file(&engine, "/b/y.txt", b"y data");

    let filter = vec!["/a/**".to_string()];
    let diff = compute_sync_diff(&engine, None, Some(&filter), true).unwrap();

    let added_paths: Vec<&str> = diff.files_added.iter().map(|f| f.path.as_str()).collect();
    assert!(
        added_paths.contains(&"/a/x.txt"),
        "expected /a/x.txt, got {:?}",
        added_paths
    );
    assert!(
        !added_paths.contains(&"/b/y.txt"),
        "/b/y.txt should be filtered out"
    );
}

// ---------------------------------------------------------------------------
// 4. test_compute_diff_excludes_system
// ---------------------------------------------------------------------------

#[test]
fn test_compute_diff_excludes_system() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_file(&engine, "/user/doc.txt", b"user doc");
    // Use a path under a known system subdirectory (config/) — these are
    // the paths `augment_with_system_subtrees` walks. Bare files directly
    // under /.aeordb-system/ (no subdirectory) are only walked for the
    // explicit list (e.g. email-config.json).
    store_file(&engine, "/.aeordb-system/config/test.json", b"system config");

    // With include_system=false
    let diff = compute_sync_diff(&engine, None, None, false).unwrap();

    let added_paths: Vec<&str> = diff.files_added.iter().map(|f| f.path.as_str()).collect();
    assert!(
        added_paths.contains(&"/user/doc.txt"),
        "user files should be present"
    );
    for path in &added_paths {
        assert!(
            !path.starts_with("/.aeordb-system"),
            "system path {} should be excluded",
            path
        );
    }

    // With include_system=true
    let diff_all = compute_sync_diff(&engine, None, None, true).unwrap();
    let all_paths: Vec<&str> = diff_all
        .files_added
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    assert!(
        all_paths.contains(&"/.aeordb-system/config/test.json"),
        "system files should be present when include_system=true, got: {:?}",
        all_paths
    );
}

// ---------------------------------------------------------------------------
// 5. test_get_needed_chunks
// ---------------------------------------------------------------------------

#[test]
fn test_get_needed_chunks() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_file(&engine, "/docs/hello.txt", b"Hello World");

    // Get the chunk hashes from the diff
    let diff = compute_sync_diff(&engine, None, None, true).unwrap();
    assert!(
        !diff.chunk_hashes_needed.is_empty(),
        "should have chunk hashes"
    );

    let chunks = get_needed_chunks(&engine, &diff.chunk_hashes_needed).unwrap();
    assert!(
        !chunks.is_empty(),
        "should return at least one chunk"
    );

    // Each returned chunk should have non-empty data
    for chunk in &chunks {
        assert!(!chunk.data.is_empty(), "chunk data should not be empty");
        assert!(!chunk.hash.is_empty(), "chunk hash should not be empty");
    }
}

// ---------------------------------------------------------------------------
// 6. test_get_needed_chunks_missing
// ---------------------------------------------------------------------------

#[test]
fn test_get_needed_chunks_missing() {
    let (engine, _temp) = create_temp_engine_for_tests();

    let fake_hash = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x11, 0x22, 0x33];
    let chunks = get_needed_chunks(&engine, &[fake_hash]).unwrap();
    assert!(chunks.is_empty(), "nonexistent hash should return empty");
}

// ---------------------------------------------------------------------------
// 7. test_apply_sync_chunks
// ---------------------------------------------------------------------------

#[test]
fn test_apply_sync_chunks() {
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    store_file(&engine_a, "/docs/hello.txt", b"Hello World");

    // Get chunks from engine A
    let diff = compute_sync_diff(&engine_a, None, None, true).unwrap();
    let chunks = get_needed_chunks(&engine_a, &diff.chunk_hashes_needed).unwrap();
    assert!(!chunks.is_empty());

    // Apply to engine B
    let stored = apply_sync_chunks(&engine_b, &chunks).unwrap();
    assert!(stored > 0, "should have stored at least one chunk");

    // Verify chunks exist in engine B
    for chunk in &chunks {
        assert!(
            engine_b.has_entry(&chunk.hash).unwrap(),
            "chunk should exist in engine B after apply"
        );
    }
}

// ---------------------------------------------------------------------------
// 8. test_apply_sync_chunks_dedup
// ---------------------------------------------------------------------------

#[test]
fn test_apply_sync_chunks_dedup() {
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    store_file(&engine_a, "/docs/hello.txt", b"Hello World");

    let diff = compute_sync_diff(&engine_a, None, None, true).unwrap();
    let chunks = get_needed_chunks(&engine_a, &diff.chunk_hashes_needed).unwrap();

    // First apply — should store
    let first = apply_sync_chunks(&engine_b, &chunks).unwrap();
    assert!(first > 0);

    // Second apply — should be 0 (dedup)
    let second = apply_sync_chunks(&engine_b, &chunks).unwrap();
    assert_eq!(second, 0, "second apply should store 0 (dedup)");
}

// ---------------------------------------------------------------------------
// 9. test_list_conflicts_typed
// ---------------------------------------------------------------------------

#[test]
fn test_list_conflicts_typed() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    // Store files for winner/loser identity hashes
    let winner_data = b"winner content";
    let loser_data = b"loser content";
    ops.store_file(&ctx, "/tmp/w", winner_data, Some("text/plain"))
        .unwrap();
    ops.store_file(&ctx, "/tmp/l", loser_data, Some("text/plain"))
        .unwrap();

    let algo = engine.hash_algo();
    let w_record = ops
        .store_file(&ctx, "/test/file.txt", winner_data, Some("text/plain"))
        .unwrap();
    let w_hash = aeordb::engine::directory_ops::file_identity_hash(
        "/test/file.txt",
        Some("text/plain"),
        &w_record.chunk_hashes,
        &algo,
    )
    .unwrap();

    let l_record = ops
        .store_file(&ctx, "/test/file_loser.txt", loser_data, Some("text/plain"))
        .unwrap();
    let l_hash = aeordb::engine::directory_ops::file_identity_hash(
        "/test/file_loser.txt",
        Some("text/plain"),
        &l_record.chunk_hashes,
        &algo,
    )
    .unwrap();

    let conflict = ConflictEntry {
        path: "/test/file.txt".to_string(),
        conflict_type: ConflictType::ConcurrentModify,
        winner: ConflictVersion {
            hash: w_hash,
            virtual_time: 10,
            node_id: 1,
            size: winner_data.len() as u64,
            content_type: Some("text/plain".to_string()),
        },
        loser: ConflictVersion {
            hash: l_hash,
            virtual_time: 5,
            node_id: 2,
            size: loser_data.len() as u64,
            content_type: Some("text/plain".to_string()),
        },
    };

    conflict_store::store_conflict(&engine, &ctx, &conflict).unwrap();

    let conflicts = list_conflicts_typed(&engine).unwrap();
    assert!(
        !conflicts.is_empty(),
        "should have at least one conflict"
    );

    let c = &conflicts[0];
    assert_eq!(c.path, "/test/file.txt");
    assert_eq!(c.winner.node_id, 1);
    assert_eq!(c.loser.node_id, 2);
    assert_eq!(c.winner.virtual_time, 10);
    assert_eq!(c.loser.virtual_time, 5);
}

// ---------------------------------------------------------------------------
// 10. test_full_library_sync_cycle
// ---------------------------------------------------------------------------

#[test]
fn test_full_library_sync_cycle() {
    // Create two independent engines (simulating two nodes)
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    // Store files on engine A
    store_file(&engine_a, "/docs/hello.txt", b"Hello from A");
    store_file(&engine_a, "/docs/world.txt", b"World from A");

    // Step 1: Compute diff on A (full diff, no base)
    let diff = compute_sync_diff(&engine_a, None, None, true).unwrap();
    assert!(diff.files_added.len() >= 2);

    // Step 2: Get chunks from A
    let chunks = get_needed_chunks(&engine_a, &diff.chunk_hashes_needed).unwrap();
    assert!(!chunks.is_empty(), "chunks should not be empty");

    // Step 3: Apply chunks to B
    let stored = apply_sync_chunks(&engine_b, &chunks).unwrap();
    assert!(stored > 0);

    // Step 4: Reconstruct files on B using the diff info
    // We need to also transfer file records and directory structures for B to
    // have the files visible. The library sync API provides chunk-level transfer;
    // file reconstruction is done via DirectoryOps.
    let ctx = RequestContext::system();
    let ops_b = DirectoryOps::new(&engine_b);

    for file_entry in &diff.files_added {
        // Skip system paths that might be in the diff
        if file_entry.path.starts_with("/.aeordb-system") {
            continue;
        }

        // Reconstruct file data from chunks
        let mut file_data = Vec::new();
        for chunk_hash in &file_entry.chunk_hashes {
            let chunk_results = get_needed_chunks(&engine_b, &[chunk_hash.clone()]).unwrap();
            if let Some(chunk) = chunk_results.first() {
                file_data.extend_from_slice(&chunk.data);
            }
        }

        ops_b
            .store_file(
                &ctx,
                &file_entry.path,
                &file_data,
                file_entry.content_type.as_deref(),
            )
            .unwrap();
    }

    // Verify B has the same files as A
    let data_hello = read_file(&engine_b, "/docs/hello.txt");
    assert_eq!(data_hello, b"Hello from A");

    let data_world = read_file(&engine_b, "/docs/world.txt");
    assert_eq!(data_world, b"World from A");
}

// ---------------------------------------------------------------------------
// Additional edge case tests
// ---------------------------------------------------------------------------

#[test]
fn test_compute_diff_empty_engine() {
    let (engine, _temp) = create_temp_engine_for_tests();

    let diff = compute_sync_diff(&engine, None, None, true).unwrap();
    // Empty engine should have no files (maybe some system entries)
    assert!(
        diff.files_added.is_empty()
            || diff
                .files_added
                .iter()
                .all(|f| f.path.starts_with("/.aeordb-system") || f.path.starts_with("/.conflicts")),
        "empty engine should have no user files"
    );
}

#[test]
fn test_compute_diff_incremental_with_modification() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_file(&engine, "/docs/a.txt", b"version 1");

    let snapshot_hash = head_hash(&engine);

    // Modify the file
    store_file(&engine, "/docs/a.txt", b"version 2");

    let diff = compute_sync_diff(&engine, Some(&snapshot_hash), None, true).unwrap();

    let modified_paths: Vec<&str> = diff
        .files_modified
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    assert!(
        modified_paths.contains(&"/docs/a.txt"),
        "modified file should appear in files_modified, got {:?}",
        modified_paths
    );
}

#[test]
fn test_compute_diff_incremental_with_deletion() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_file(&engine, "/docs/a.txt", b"to be deleted");
    store_file(&engine, "/docs/b.txt", b"to keep");

    let snapshot_hash = head_hash(&engine);

    // Delete a.txt
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.delete_file(&ctx, "/docs/a.txt").unwrap();

    let diff = compute_sync_diff(&engine, Some(&snapshot_hash), None, true).unwrap();

    let deleted_paths: Vec<&str> = diff
        .files_deleted
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    assert!(
        deleted_paths.contains(&"/docs/a.txt"),
        "deleted file should appear in files_deleted, got {:?}",
        deleted_paths
    );
}

#[test]
fn test_compute_diff_with_symlinks() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_file(&engine, "/docs/target.txt", b"target content");
    store_symlink(&engine, "/docs/link", "/docs/target.txt");

    let diff = compute_sync_diff(&engine, None, None, true).unwrap();

    let symlink_paths: Vec<&str> = diff
        .symlinks_added
        .iter()
        .map(|s| s.path.as_str())
        .collect();
    assert!(
        symlink_paths.contains(&"/docs/link"),
        "symlink should appear in symlinks_added, got {:?}",
        symlink_paths
    );

    // Verify the target is correct
    let link = diff
        .symlinks_added
        .iter()
        .find(|s| s.path == "/docs/link")
        .unwrap();
    assert_eq!(link.target, "/docs/target.txt");
}

#[test]
fn test_paths_filter_with_multiple_patterns() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_file(&engine, "/a/x.txt", b"ax");
    store_file(&engine, "/b/y.txt", b"by");
    store_file(&engine, "/c/z.txt", b"cz");

    let filter = vec!["/a/**".to_string(), "/c/**".to_string()];
    let diff = compute_sync_diff(&engine, None, Some(&filter), true).unwrap();

    let added_paths: Vec<&str> = diff.files_added.iter().map(|f| f.path.as_str()).collect();
    assert!(added_paths.contains(&"/a/x.txt"));
    assert!(added_paths.contains(&"/c/z.txt"));
    assert!(!added_paths.contains(&"/b/y.txt"));
}

#[test]
fn test_paths_filter_empty_patterns() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_file(&engine, "/docs/a.txt", b"alpha");

    // Empty filter means no filtering (all pass)
    let filter: Vec<String> = vec![];
    let diff = compute_sync_diff(&engine, None, Some(&filter), true).unwrap();

    // With empty patterns, everything passes through (the filter is a no-op)
    assert!(!diff.files_added.is_empty());
}

#[test]
fn test_get_needed_chunks_partial_missing() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_file(&engine, "/docs/hello.txt", b"Hello World");

    let diff = compute_sync_diff(&engine, None, None, true).unwrap();
    assert!(!diff.chunk_hashes_needed.is_empty());

    // Mix real hashes with a fake one
    let mut hashes = diff.chunk_hashes_needed.clone();
    hashes.push(vec![0xFF; 32]); // fake hash

    let chunks = get_needed_chunks(&engine, &hashes).unwrap();
    // Should return only the real chunks, silently skipping the missing one
    assert_eq!(
        chunks.len(),
        diff.chunk_hashes_needed.len(),
        "should return only existing chunks"
    );
}

#[test]
fn test_apply_sync_chunks_empty() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let stored = apply_sync_chunks(&engine, &[]).unwrap();
    assert_eq!(stored, 0, "applying empty chunks should store 0");
}

#[test]
fn test_sync_diff_file_entry_has_chunk_hashes() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_file(&engine, "/docs/hello.txt", b"Hello World");

    let diff = compute_sync_diff(&engine, None, None, true).unwrap();

    let hello = diff
        .files_added
        .iter()
        .find(|f| f.path == "/docs/hello.txt")
        .expect("should find hello.txt in added");

    assert!(!hello.chunk_hashes.is_empty(), "file entry should have chunk_hashes");
    assert!(hello.size > 0, "file entry should have non-zero size");
    assert!(!hello.hash.is_empty(), "file entry should have a hash");
}
