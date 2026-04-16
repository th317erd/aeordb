use aeordb::engine::conflict_store;
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::merge::{ConflictEntry, ConflictType, ConflictVersion};
use aeordb::engine::RequestContext;
use aeordb::server::create_temp_engine_for_tests;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a ConflictEntry for testing with known data stored in the engine.
fn make_test_conflict(
    engine: &aeordb::engine::StorageEngine,
    path: &str,
    winner_data: &[u8],
    loser_data: &[u8],
) -> ConflictEntry {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);

    // Store winner file at a temporary path to get its identity hash
    let winner_path = format!("/tmp/winner{}", path);
    let winner_record = ops
        .store_file(&ctx, &winner_path, winner_data, Some("text/plain"))
        .unwrap();

    // Store loser file at a temporary path to get its identity hash
    let loser_path = format!("/tmp/loser{}", path);
    let loser_record = ops
        .store_file(&ctx, &loser_path, loser_data, Some("text/plain"))
        .unwrap();

    // Compute identity hashes (same as what merge.rs produces)
    let algo = engine.hash_algo();
    let winner_hash = aeordb::engine::directory_ops::file_identity_hash(
        &winner_path,
        winner_record.content_type.as_deref(),
        &winner_record.chunk_hashes,
        &algo,
    )
    .unwrap();
    let loser_hash = aeordb::engine::directory_ops::file_identity_hash(
        &loser_path,
        loser_record.content_type.as_deref(),
        &loser_record.chunk_hashes,
        &algo,
    )
    .unwrap();

    // Store the winner at the real path (simulating merge auto-winner)
    ops.store_file(&ctx, path, winner_data, Some("text/plain"))
        .unwrap();

    ConflictEntry {
        path: path.to_string(),
        conflict_type: ConflictType::ConcurrentModify,
        winner: ConflictVersion {
            hash: winner_hash,
            virtual_time: 200,
            node_id: 1,
            size: winner_data.len() as u64,
            content_type: Some("text/plain".to_string()),
        },
        loser: ConflictVersion {
            hash: loser_hash,
            virtual_time: 100,
            node_id: 2,
            size: loser_data.len() as u64,
            content_type: Some("text/plain".to_string()),
        },
    }
}

// ===========================================================================
// test_store_and_get_conflict
// ===========================================================================

#[test]
fn test_store_and_get_conflict() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();

    let conflict = make_test_conflict(&engine, "/docs/file.txt", b"winner v1", b"loser v1");
    conflict_store::store_conflict(&engine, &ctx, &conflict).unwrap();

    let result = conflict_store::get_conflict(&engine, "/docs/file.txt").unwrap();
    assert!(result.is_some(), "conflict should exist");

    let meta = result.unwrap();
    assert_eq!(meta["path"], "/docs/file.txt");
    assert_eq!(meta["conflict_type"], "ConcurrentModify");
    assert_eq!(meta["auto_winner"], "winner");
    assert!(meta["created_at"].as_i64().is_some());

    // Winner metadata
    assert!(meta["winner"]["hash"].as_str().is_some());
    assert_eq!(meta["winner"]["virtual_time"], 200);
    assert_eq!(meta["winner"]["node_id"], 1);
    assert_eq!(meta["winner"]["size"], 9); // "winner v1" = 9 bytes

    // Loser metadata
    assert!(meta["loser"]["hash"].as_str().is_some());
    assert_eq!(meta["loser"]["virtual_time"], 100);
    assert_eq!(meta["loser"]["node_id"], 2);
    assert_eq!(meta["loser"]["size"], 8); // "loser v1" = 8 bytes
}

// ===========================================================================
// test_list_conflicts
// ===========================================================================

#[test]
fn test_list_conflicts() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();

    let conflict1 = make_test_conflict(&engine, "/docs/a.txt", b"winner-a", b"loser-a");
    let conflict2 = make_test_conflict(&engine, "/docs/b.txt", b"winner-b", b"loser-b");
    conflict_store::store_conflict(&engine, &ctx, &conflict1).unwrap();
    conflict_store::store_conflict(&engine, &ctx, &conflict2).unwrap();

    let conflicts = conflict_store::list_conflicts(&engine).unwrap();
    assert_eq!(conflicts.len(), 2, "should have 2 conflicts");

    let paths: Vec<&str> = conflicts
        .iter()
        .filter_map(|c| c["path"].as_str())
        .collect();
    assert!(paths.contains(&"/docs/a.txt"));
    assert!(paths.contains(&"/docs/b.txt"));
}

// ===========================================================================
// test_list_no_conflicts
// ===========================================================================

#[test]
fn test_list_no_conflicts() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let conflicts = conflict_store::list_conflicts(&engine).unwrap();
    assert!(conflicts.is_empty(), "should be empty when no conflicts");
}

// ===========================================================================
// test_dismiss_conflict
// ===========================================================================

#[test]
fn test_dismiss_conflict() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();

    let conflict = make_test_conflict(&engine, "/docs/dismiss.txt", b"winner", b"loser");
    conflict_store::store_conflict(&engine, &ctx, &conflict).unwrap();

    // Verify it exists
    assert!(conflict_store::get_conflict(&engine, "/docs/dismiss.txt")
        .unwrap()
        .is_some());

    // Dismiss
    conflict_store::dismiss_conflict(&engine, &ctx, "/docs/dismiss.txt").unwrap();

    // Should be gone
    assert!(conflict_store::get_conflict(&engine, "/docs/dismiss.txt")
        .unwrap()
        .is_none());
}

// ===========================================================================
// test_conflict_not_found
// ===========================================================================

#[test]
fn test_conflict_not_found() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let result = conflict_store::get_conflict(&engine, "/nonexistent/path.txt").unwrap();
    assert!(result.is_none(), "nonexistent conflict should return None");
}

// ===========================================================================
// test_dismiss_nonexistent_conflict
// ===========================================================================

#[test]
fn test_dismiss_nonexistent_conflict() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let result = conflict_store::dismiss_conflict(&engine, &ctx, "/nonexistent.txt");
    assert!(result.is_err(), "dismiss nonexistent should error");
}

// ===========================================================================
// test_resolve_conflict_invalid_pick
// ===========================================================================

#[test]
fn test_resolve_conflict_invalid_pick() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();

    let conflict = make_test_conflict(&engine, "/docs/pick.txt", b"winner-data", b"loser-data");
    conflict_store::store_conflict(&engine, &ctx, &conflict).unwrap();

    let result = conflict_store::resolve_conflict(&engine, &ctx, "/docs/pick.txt", "neither");
    assert!(result.is_err(), "invalid pick should error");
}

// ===========================================================================
// test_resolve_nonexistent_conflict
// ===========================================================================

#[test]
fn test_resolve_nonexistent_conflict() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let result = conflict_store::resolve_conflict(&engine, &ctx, "/nonexistent.txt", "winner");
    assert!(result.is_err(), "resolve nonexistent should error");
}

// ===========================================================================
// test_store_multiple_conflict_types
// ===========================================================================

#[test]
fn test_store_modify_delete_conflict() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();

    // ModifyDelete conflict: loser has empty hash (deleted)
    let ops = DirectoryOps::new(&engine);
    let winner_record = ops
        .store_file(&ctx, "/docs/md.txt", b"modified", Some("text/plain"))
        .unwrap();
    let algo = engine.hash_algo();
    let winner_hash = aeordb::engine::directory_ops::file_identity_hash(
        "/tmp/winner/docs/md.txt",
        winner_record.content_type.as_deref(),
        &winner_record.chunk_hashes,
        &algo,
    )
    .unwrap();

    let conflict = ConflictEntry {
        path: "/docs/md.txt".to_string(),
        conflict_type: ConflictType::ModifyDelete,
        winner: ConflictVersion {
            hash: winner_hash,
            virtual_time: 300,
            node_id: 1,
            size: 8,
            content_type: Some("text/plain".to_string()),
        },
        loser: ConflictVersion {
            hash: Vec::new(),
            virtual_time: 0,
            node_id: 0,
            size: 0,
            content_type: None,
        },
    };

    conflict_store::store_conflict(&engine, &ctx, &conflict).unwrap();

    let meta = conflict_store::get_conflict(&engine, "/docs/md.txt")
        .unwrap()
        .expect("conflict should exist");
    assert_eq!(meta["conflict_type"], "ModifyDelete");
    assert_eq!(meta["loser"]["hash"], "");
}

// ===========================================================================
// test_store_concurrent_create_conflict
// ===========================================================================

#[test]
fn test_store_concurrent_create_conflict() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();

    let mut conflict = make_test_conflict(&engine, "/new/file.txt", b"version-a", b"version-b");
    conflict.conflict_type = ConflictType::ConcurrentCreate;

    conflict_store::store_conflict(&engine, &ctx, &conflict).unwrap();

    let meta = conflict_store::get_conflict(&engine, "/new/file.txt")
        .unwrap()
        .expect("conflict should exist");
    assert_eq!(meta["conflict_type"], "ConcurrentCreate");
}

// ===========================================================================
// test_list_conflicts_with_nested_paths
// ===========================================================================

#[test]
fn test_list_conflicts_with_nested_paths() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();

    let c1 = make_test_conflict(&engine, "/a/deep/path/file.txt", b"w1", b"l1");
    let c2 = make_test_conflict(&engine, "/b/another/file.json", b"w2", b"l2");
    conflict_store::store_conflict(&engine, &ctx, &c1).unwrap();
    conflict_store::store_conflict(&engine, &ctx, &c2).unwrap();

    let conflicts = conflict_store::list_conflicts(&engine).unwrap();
    assert_eq!(conflicts.len(), 2);
}

// ===========================================================================
// test_overwrite_existing_conflict
// ===========================================================================

#[test]
fn test_overwrite_existing_conflict() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();

    let c1 = make_test_conflict(&engine, "/docs/overwrite.txt", b"winner-v1", b"loser-v1");
    conflict_store::store_conflict(&engine, &ctx, &c1).unwrap();

    // Store a new conflict for the same path
    let c2 = make_test_conflict(&engine, "/docs/overwrite.txt", b"winner-v2", b"loser-v2");
    conflict_store::store_conflict(&engine, &ctx, &c2).unwrap();

    // Should only have one conflict for this path (overwritten)
    let meta = conflict_store::get_conflict(&engine, "/docs/overwrite.txt")
        .unwrap()
        .expect("conflict should exist");
    // The latest store should win
    assert_eq!(meta["winner"]["size"], 9); // "winner-v2" = 9 bytes
}
