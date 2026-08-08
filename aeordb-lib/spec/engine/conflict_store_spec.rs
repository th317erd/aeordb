use aeordb::engine::conflict_store;
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::gc::run_gc;
use aeordb::engine::merge::{ConflictEntry, ConflictType, ConflictVersion};
use aeordb::engine::{EngineError, EntryType, RequestContext};
use aeordb::server::create_temp_engine_for_tests;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a ConflictEntry for testing with known data stored in the engine.
fn make_test_conflict(engine: &aeordb::engine::StorageEngine, path: &str, winner_data: &[u8], loser_data: &[u8]) -> ConflictEntry {
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine);

  // Publish both versions at the contested path so their immutable identities
  // model records emitted by a real three-way merge.
  let winner_record = ops.store_file_buffered(&ctx, path, winner_data, Some("text/plain")).unwrap();
  let loser_record = ops.store_file_buffered(&ctx, path, loser_data, Some("text/plain")).unwrap();

  // Compute identity hashes (same as what merge.rs produces)
  let algo = engine.hash_algo();
  let winner_hash =
    aeordb::engine::directory_ops::file_identity_hash(path, winner_record.content_type.as_deref(), &winner_record.chunk_hashes, &algo)
      .unwrap();
  let loser_hash =
    aeordb::engine::directory_ops::file_identity_hash(path, loser_record.content_type.as_deref(), &loser_record.chunk_hashes, &algo)
      .unwrap();

  // Store the winner at the real path (simulating merge auto-winner)
  ops.store_file_buffered(&ctx, path, winner_data, Some("text/plain")).unwrap();

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

fn rewrite_conflict_metadata(engine: &aeordb::engine::StorageEngine, path: &str, mutate: impl FnOnce(&mut serde_json::Value)) {
  let context = RequestContext::system();
  let operations = DirectoryOps::new(engine);
  let metadata_path = format!("/.aeordb-conflicts{path}/.meta");
  let mut metadata: serde_json::Value = serde_json::from_slice(&operations.read_file_buffered(&metadata_path).unwrap()).unwrap();
  mutate(&mut metadata);
  operations.store_file_buffered(&context, &metadata_path, &serde_json::to_vec(&metadata).unwrap(), Some("application/json")).unwrap();
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

#[test]
fn store_conflict_rejects_a_file_at_its_metadata_directory_path() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let conflict = make_test_conflict(&engine, "/blocked.txt", b"winner", b"loser");
  let ancestor = "/.aeordb-conflicts/blocked.txt";
  ops.store_file_buffered(&ctx, ancestor, b"must remain a file", Some("text/plain")).unwrap();

  let error = conflict_store::store_conflict(&engine, &ctx, &conflict).expect_err("a conflict directory must not replace a file ancestor");

  assert!(matches!(error, EngineError::AlreadyExists(_)), "unexpected error: {error}");
  assert_eq!(ops.read_file_buffered(ancestor).unwrap(), b"must remain a file");
  assert!(conflict_store::get_conflict(&engine, &conflict.path).unwrap().is_none());
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

  let paths: Vec<&str> = conflicts.iter().filter_map(|c| c["path"].as_str()).collect();
  assert!(paths.contains(&"/docs/a.txt"));
  assert!(paths.contains(&"/docs/b.txt"));
}

#[test]
fn list_conflicts_rejects_malformed_authoritative_metadata() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  DirectoryOps::new(&engine)
    .store_file_buffered(&ctx, "/.aeordb-conflicts/docs/broken/.meta", b"not json", Some("application/json"))
    .unwrap();

  let error = conflict_store::list_conflicts(&engine).expect_err("conflict authority must not omit malformed metadata");
  assert!(error.to_string().contains("JSON parse error"), "unexpected error: {error}");
}

#[test]
fn gc_fails_closed_before_sweep_when_conflict_references_are_malformed() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let conflict = make_test_conflict(&engine, "/docs/gc-malformed.txt", b"winner", b"loser");
  conflict_store::store_conflict(&engine, &context, &conflict).unwrap();
  rewrite_conflict_metadata(&engine, &conflict.path, |metadata| metadata["loser"]["hash"] = serde_json::json!("not hex"));
  let unreachable_key = engine.compute_hash(b"must survive aborted conflict-aware GC").unwrap();
  engine.store_entry(EntryType::Chunk, &unreachable_key, b"unreachable before failed mark").unwrap();

  let error = run_gc(&engine, &context, false).expect_err("malformed conflict authority must abort mark before sweep");

  assert!(matches!(error, EngineError::InvalidInput(_) | EngineError::CorruptEntry { .. }), "unexpected error: {error}");
  assert!(engine.has_entry(&unreachable_key).unwrap(), "failed mark must not sweep unrelated entries");
  assert!(conflict_store::get_conflict(&engine, &conflict.path).unwrap().is_some());
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
  assert!(conflict_store::get_conflict(&engine, "/docs/dismiss.txt").unwrap().is_some());

  // Dismiss
  conflict_store::dismiss_conflict(&engine, &ctx, "/docs/dismiss.txt").unwrap();

  // Should be gone
  assert!(conflict_store::get_conflict(&engine, "/docs/dismiss.txt").unwrap().is_none());
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

#[test]
fn resolve_conflict_publishes_the_selected_version_and_removes_evidence() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let conflict = make_test_conflict(&engine, "/docs/resolved.txt", b"winner bytes", b"loser bytes");
  conflict_store::store_conflict(&engine, &context, &conflict).unwrap();

  conflict_store::resolve_conflict(&engine, &context, &conflict.path, "loser").unwrap();

  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(&conflict.path).unwrap(), b"loser bytes");
  assert!(conflict_store::get_conflict(&engine, &conflict.path).unwrap().is_none());
}

#[test]
fn resolve_modify_delete_conflict_can_select_file_deletion() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let mut conflict = make_test_conflict(&engine, "/docs/delete-selected.txt", b"winner bytes", b"unused loser bytes");
  conflict.conflict_type = ConflictType::ModifyDelete;
  conflict.loser = ConflictVersion { hash: Vec::new(), virtual_time: 0, node_id: 0, size: 0, content_type: None };
  conflict_store::store_conflict(&engine, &context, &conflict).unwrap();

  conflict_store::resolve_conflict(&engine, &context, &conflict.path, "loser").unwrap();

  assert!(DirectoryOps::new(&engine).read_file_buffered(&conflict.path).is_err());
  assert!(conflict_store::get_conflict(&engine, &conflict.path).unwrap().is_none());
}

#[test]
fn resolve_symlink_conflict_can_select_the_losing_target() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let contested_path = "/links/contested";
  ops.store_symlink(&context, contested_path, "/targets/winner").unwrap();
  ops.store_symlink(&context, contested_path, "/targets/loser").unwrap();
  ops.store_symlink(&context, contested_path, "/targets/winner").unwrap();
  let conflict = ConflictEntry {
    path: contested_path.to_string(),
    conflict_type: ConflictType::ConcurrentModify,
    winner: ConflictVersion {
      hash: aeordb::engine::symlink_identity_hash(contested_path, "/targets/winner", &engine.hash_algo()).unwrap(),
      virtual_time: 2,
      node_id: 1,
      size: 0,
      content_type: None,
    },
    loser: ConflictVersion {
      hash: aeordb::engine::symlink_identity_hash(contested_path, "/targets/loser", &engine.hash_algo()).unwrap(),
      virtual_time: 1,
      node_id: 2,
      size: 0,
      content_type: None,
    },
  };
  conflict_store::store_conflict(&engine, &context, &conflict).unwrap();

  conflict_store::resolve_conflict(&engine, &context, &conflict.path, "loser").unwrap();

  assert_eq!(ops.get_symlink(&conflict.path).unwrap().unwrap().target, "/targets/loser");
  assert!(conflict_store::get_conflict(&engine, &conflict.path).unwrap().is_none());
}

#[test]
fn resolve_modify_delete_conflict_can_select_symlink_deletion() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let contested_path = "/links/delete-selected";
  ops.store_symlink(&context, contested_path, "/targets/winner").unwrap();
  let conflict = ConflictEntry {
    path: contested_path.to_string(),
    conflict_type: ConflictType::ModifyDelete,
    winner: ConflictVersion {
      hash: aeordb::engine::symlink_identity_hash(contested_path, "/targets/winner", &engine.hash_algo()).unwrap(),
      virtual_time: 2,
      node_id: 1,
      size: 0,
      content_type: None,
    },
    loser: ConflictVersion { hash: Vec::new(), virtual_time: 0, node_id: 0, size: 0, content_type: None },
  };
  conflict_store::store_conflict(&engine, &context, &conflict).unwrap();

  conflict_store::resolve_conflict(&engine, &context, &conflict.path, "loser").unwrap();

  assert!(ops.get_symlink(&conflict.path).unwrap().is_none());
  assert!(conflict_store::get_conflict(&engine, &conflict.path).unwrap().is_none());
}

#[test]
fn resolve_conflict_rejects_missing_chosen_file_record_and_preserves_evidence() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let conflict = ConflictEntry {
    path: "/missing-chosen.txt".to_string(),
    conflict_type: ConflictType::ConcurrentModify,
    winner: ConflictVersion {
      hash: vec![0xA5; engine.hash_algo().hash_length()],
      virtual_time: 2,
      node_id: 1,
      size: 10,
      content_type: Some("text/plain".to_string()),
    },
    loser: ConflictVersion {
      hash: vec![0x5A; engine.hash_algo().hash_length()],
      virtual_time: 1,
      node_id: 2,
      size: 9,
      content_type: Some("text/plain".to_string()),
    },
  };
  aeordb::engine::conflict_store::store_conflict(&engine, &context, &conflict).unwrap();

  let error = aeordb::engine::conflict_store::resolve_conflict(&engine, &context, &conflict.path, "winner").unwrap_err();

  assert!(matches!(error, aeordb::engine::EngineError::NotFound(_)), "unexpected error: {error}");
  assert!(aeordb::engine::conflict_store::get_conflict(&engine, &conflict.path).unwrap().is_some());
}

#[test]
fn resolve_conflict_rejects_a_corrupt_chosen_chunk_and_preserves_target_and_evidence() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let conflict = make_test_conflict(&engine, "/corrupt/chosen-chunk.txt", b"winner bytes", b"loser bytes");
  aeordb::engine::conflict_store::store_conflict(&engine, &context, &conflict).unwrap();
  let target_before = DirectoryOps::new(&engine).read_file_buffered(&conflict.path).unwrap();
  let (header, _, data) = engine.get_entry(&conflict.loser.hash).unwrap().unwrap();
  let record = aeordb::engine::FileRecord::deserialize(&data, engine.hash_algo().hash_length(), header.entry_version).unwrap();
  engine.store_entry(aeordb::engine::EntryType::Chunk, &record.chunk_hashes[0], b"corrupt chosen bytes").unwrap();

  let error = aeordb::engine::conflict_store::resolve_conflict(&engine, &context, &conflict.path, "loser").unwrap_err();

  assert!(matches!(error, aeordb::engine::EngineError::CorruptEntry { .. }), "unexpected error: {error}");
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(&conflict.path).unwrap(), target_before);
  assert!(aeordb::engine::conflict_store::get_conflict(&engine, &conflict.path).unwrap().is_some());
}

#[test]
fn dismiss_conflict_surfaces_cleanup_failure_and_preserves_evidence() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let conflict = make_test_conflict(&engine, "/cleanup/failure.txt", b"winner", b"loser");
  aeordb::engine::conflict_store::store_conflict(&engine, &context, &conflict).unwrap();
  let parent_key = aeordb::engine::directory_path_hash("/.aeordb-conflicts/cleanup/failure.txt", &engine.hash_algo()).unwrap();
  engine.store_entry(aeordb::engine::EntryType::Chunk, &parent_key, b"wrong type").unwrap();

  let error = aeordb::engine::conflict_store::dismiss_conflict(&engine, &context, &conflict.path).unwrap_err();

  assert!(!matches!(error, aeordb::engine::EngineError::NotFound(_)), "cleanup failure must not be treated as idempotent: {error}");
  assert!(aeordb::engine::conflict_store::get_conflict(&engine, &conflict.path).unwrap().is_some());
}

#[test]
fn resolve_conflict_cleanup_failure_preserves_target_and_evidence() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let conflict = make_test_conflict(&engine, "/cleanup/resolve.txt", b"winner", b"loser");
  aeordb::engine::conflict_store::store_conflict(&engine, &context, &conflict).unwrap();
  let target_before = DirectoryOps::new(&engine).read_file_buffered(&conflict.path).unwrap();
  let parent_key = aeordb::engine::directory_path_hash("/.aeordb-conflicts/cleanup/resolve.txt", &engine.hash_algo()).unwrap();
  engine.store_entry(aeordb::engine::EntryType::Chunk, &parent_key, b"wrong type").unwrap();

  let error = aeordb::engine::conflict_store::resolve_conflict(&engine, &context, &conflict.path, "loser").unwrap_err();

  assert!(!matches!(error, aeordb::engine::EngineError::NotFound(_)), "cleanup failure must remain observable: {error}");
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(&conflict.path).unwrap(), target_before);
  assert!(aeordb::engine::conflict_store::get_conflict(&engine, &conflict.path).unwrap().is_some());
}

#[test]
fn resolve_conflict_rejects_transplanted_metadata_and_preserves_target_and_evidence() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let conflict = make_test_conflict(&engine, "/tampered/path.txt", b"winner", b"loser");
  aeordb::engine::conflict_store::store_conflict(&engine, &context, &conflict).unwrap();
  let target_before = DirectoryOps::new(&engine).read_file_buffered(&conflict.path).unwrap();
  rewrite_conflict_metadata(&engine, &conflict.path, |metadata| metadata["path"] = serde_json::json!("/different/path.txt"));

  let error = aeordb::engine::conflict_store::resolve_conflict(&engine, &context, &conflict.path, "loser").unwrap_err();

  assert!(matches!(error, aeordb::engine::EngineError::CorruptEntry { .. }), "unexpected error: {error}");
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(&conflict.path).unwrap(), target_before);
  assert!(aeordb::engine::conflict_store::get_conflict(&engine, &conflict.path).unwrap().is_some());
}

#[test]
fn resolve_conflict_rejects_a_version_identity_bound_to_a_different_path() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let operations = DirectoryOps::new(&engine);
  let conflict = make_test_conflict(&engine, "/tampered/version.txt", b"winner", b"loser");
  aeordb::engine::conflict_store::store_conflict(&engine, &context, &conflict).unwrap();
  let target_before = operations.read_file_buffered(&conflict.path).unwrap();
  let unrelated = operations.store_file_buffered(&context, "/unrelated.txt", b"other", Some("text/plain")).unwrap();
  let unrelated_hash =
    aeordb::engine::file_identity_hash(&unrelated.path, unrelated.content_type.as_deref(), &unrelated.chunk_hashes, &engine.hash_algo())
      .unwrap();
  rewrite_conflict_metadata(&engine, &conflict.path, |metadata| {
    metadata["loser"]["hash"] = serde_json::json!(hex::encode(unrelated_hash));
  });

  let error = aeordb::engine::conflict_store::resolve_conflict(&engine, &context, &conflict.path, "loser").unwrap_err();

  assert!(matches!(error, aeordb::engine::EngineError::CorruptEntry { .. }), "unexpected error: {error}");
  assert_eq!(operations.read_file_buffered(&conflict.path).unwrap(), target_before);
  assert!(aeordb::engine::conflict_store::get_conflict(&engine, &conflict.path).unwrap().is_some());
}

#[test]
fn resolve_conflict_rejects_unknown_conflict_type_and_preserves_target_and_evidence() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let conflict = make_test_conflict(&engine, "/tampered/type.txt", b"winner", b"loser");
  aeordb::engine::conflict_store::store_conflict(&engine, &context, &conflict).unwrap();
  let target_before = DirectoryOps::new(&engine).read_file_buffered(&conflict.path).unwrap();
  rewrite_conflict_metadata(&engine, &conflict.path, |metadata| metadata["conflict_type"] = serde_json::json!("InventedConflict"));

  let error = aeordb::engine::conflict_store::resolve_conflict(&engine, &context, &conflict.path, "loser").unwrap_err();

  assert!(matches!(error, aeordb::engine::EngineError::CorruptEntry { .. }), "unexpected error: {error}");
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(&conflict.path).unwrap(), target_before);
  assert!(aeordb::engine::conflict_store::get_conflict(&engine, &conflict.path).unwrap().is_some());
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
  let winner_record = ops.store_file_buffered(&ctx, "/docs/md.txt", b"modified", Some("text/plain")).unwrap();
  let algo = engine.hash_algo();
  let winner_hash = aeordb::engine::directory_ops::file_identity_hash(
    "/docs/md.txt",
    winner_record.content_type.as_deref(),
    &winner_record.chunk_hashes,
    &algo,
  )
  .unwrap();

  let conflict = ConflictEntry {
    path: "/docs/md.txt".to_string(),
    conflict_type: ConflictType::ModifyDelete,
    winner: ConflictVersion { hash: winner_hash, virtual_time: 300, node_id: 1, size: 8, content_type: Some("text/plain".to_string()) },
    loser: ConflictVersion { hash: Vec::new(), virtual_time: 0, node_id: 0, size: 0, content_type: None },
  };

  conflict_store::store_conflict(&engine, &ctx, &conflict).unwrap();

  let meta = conflict_store::get_conflict(&engine, "/docs/md.txt").unwrap().expect("conflict should exist");
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

  let meta = conflict_store::get_conflict(&engine, "/new/file.txt").unwrap().expect("conflict should exist");
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
  let meta = conflict_store::get_conflict(&engine, "/docs/overwrite.txt").unwrap().expect("conflict should exist");
  // The latest store should win
  assert_eq!(meta["winner"]["size"], 9); // "winner-v2" = 9 bytes
}
