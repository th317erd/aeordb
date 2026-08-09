use aeordb::engine::file_record::FileRecord;
use aeordb::engine::merge::MergeOp;
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
use aeordb::engine::symlink_record::SymlinkRecord;
use aeordb::engine::sync_apply::apply_merge_operations;
use aeordb::engine::{DirectoryOps, EngineError, RequestContext, StorageEngine};
use aeordb::server::create_temp_engine_for_tests;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Store a file and return its FileRecord (retrieved from the engine).
fn store_and_get_record(engine: &StorageEngine, path: &str, data: &[u8]) -> (Vec<u8>, FileRecord) {
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
  let (_, mut file_record) = store_and_get_record(&engine, "/source.txt", b"hello world");

  // Delete the original file so we can test adding via merge
  let ops = DirectoryOps::new(&engine);
  ops.delete_file(&context, "/source.txt").unwrap();

  // Now apply a merge operation to add a correctly path-bound record using those chunks
  file_record.path = "/merged.txt".to_string();
  let file_hash = aeordb::engine::file_identity_hash(
    &file_record.path,
    file_record.content_type.as_deref(),
    &file_record.chunk_hashes,
    &engine.hash_algo(),
  )
  .unwrap();
  let operations = vec![MergeOp::AddFile { path: "/merged.txt".to_string(), file_hash, file_record }];

  apply_merge_operations(&engine, &context, &operations).unwrap();

  // Verify the file exists
  let head = engine.head_hash().unwrap();
  let tree = aeordb::engine::tree_walker::walk_version_tree(&engine, &head).unwrap();
  assert!(tree.files.contains_key("/merged.txt"), "merged file should exist");
}

#[test]
fn sync_apply_backfills_a_legacy_empty_content_hash_before_v1_publication() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let (_, mut file_record) = store_and_get_record(&engine, "/legacy-source.txt", b"legacy sync bytes");
  file_record.path = "/legacy-received.txt".to_string();
  file_record.content_hash.clear();
  let file_hash = aeordb::engine::file_identity_hash(
    &file_record.path,
    file_record.content_type.as_deref(),
    &file_record.chunk_hashes,
    &engine.hash_algo(),
  )
  .unwrap();

  apply_merge_operations(&engine, &context, &[MergeOp::AddFile { path: file_record.path.clone(), file_hash, file_record }]).unwrap();

  let stored = DirectoryOps::new(&engine).get_metadata("/legacy-received.txt").unwrap().unwrap();
  assert_eq!(stored.content_hash, aeordb::engine::whole_file_content_hash(b"legacy sync bytes", &engine.hash_algo()).unwrap());
}

#[test]
fn sync_apply_preserves_remote_file_timestamps_for_merge_ordering() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let (_, mut file_record) = store_and_get_record(&engine, "/timestamp-source.txt", b"timestamp bytes");
  file_record.path = "/timestamp-received.txt".to_string();
  file_record.created_at = 1_000;
  file_record.updated_at = 2_000;
  file_record.metadata = b"remote-file-record-metadata".to_vec();
  let file_hash = aeordb::engine::file_identity_hash(
    &file_record.path,
    file_record.content_type.as_deref(),
    &file_record.chunk_hashes,
    &engine.hash_algo(),
  )
  .unwrap();

  apply_merge_operations(&engine, &context, &[MergeOp::AddFile { path: file_record.path.clone(), file_hash, file_record }]).unwrap();

  let stored = DirectoryOps::new(&engine).get_metadata("/timestamp-received.txt").unwrap().unwrap();
  assert_eq!(stored.created_at, 1_000);
  assert_eq!(stored.updated_at, 2_000);
  assert_eq!(stored.metadata, b"remote-file-record-metadata");
}

#[test]
fn sync_apply_rejects_a_file_record_for_a_different_path_before_publication() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let (file_hash, file_record) = store_and_get_record(&engine, "/signed-source.txt", b"path-bound bytes");

  let error =
    apply_merge_operations(&engine, &context, &[MergeOp::AddFile { path: "/transplanted.txt".to_string(), file_hash, file_record }])
      .unwrap_err();

  assert!(matches!(error, EngineError::InvalidInput(message) if message.contains("does not match")));
  assert!(DirectoryOps::new(&engine).get_metadata("/transplanted.txt").unwrap().is_none());
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
  let operations = vec![MergeOp::DeleteFile { path: "/to_delete.txt".to_string() }];

  apply_merge_operations(&engine, &context, &operations).unwrap();

  // Verify file is gone
  let head = engine.head_hash().unwrap();
  let tree = aeordb::engine::tree_walker::walk_version_tree(&engine, &head).unwrap();
  assert!(!tree.files.contains_key("/to_delete.txt"), "file should be deleted");
}

#[test]
fn test_apply_delete_nonexistent_file_succeeds() {
  // Deleting a file that doesn't exist should NOT error
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();

  let operations = vec![MergeOp::DeleteFile { path: "/does_not_exist.txt".to_string() }];

  let result = apply_merge_operations(&engine, &context, &operations);
  assert!(result.is_ok(), "deleting nonexistent file should not fail");
}

#[test]
fn test_apply_adds_symlink() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();

  let symlink_record = SymlinkRecord { path: "/link".to_string(), target: "/some/target".to_string(), created_at: 1000, updated_at: 1000 };
  let symlink_hash = aeordb::engine::symlink_identity_hash(&symlink_record.path, &symlink_record.target, &engine.hash_algo()).unwrap();

  let operations = vec![MergeOp::AddSymlink { path: "/link".to_string(), symlink_hash, symlink_record }];

  apply_merge_operations(&engine, &context, &operations).unwrap();

  // Verify symlink exists
  let head = engine.head_hash().unwrap();
  let tree = aeordb::engine::tree_walker::walk_version_tree(&engine, &head).unwrap();
  assert!(tree.symlinks.contains_key("/link"), "symlink should exist");
  assert_eq!(tree.symlinks["/link"].1.target, "/some/target");
}

#[test]
fn sync_apply_rejects_a_symlink_record_for_a_different_path_before_publication() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let symlink_record =
    SymlinkRecord { path: "/signed-link".to_string(), target: "/target".to_string(), created_at: 1000, updated_at: 1000 };
  let symlink_hash = aeordb::engine::symlink_identity_hash(&symlink_record.path, &symlink_record.target, &engine.hash_algo()).unwrap();

  let error = apply_merge_operations(
    &engine,
    &context,
    &[MergeOp::AddSymlink { path: "/transplanted-link".to_string(), symlink_hash, symlink_record }],
  )
  .unwrap_err();

  assert!(matches!(error, EngineError::InvalidInput(message) if message.contains("does not match")));
  assert!(DirectoryOps::new(&engine).get_symlink("/transplanted-link").unwrap().is_none());
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
  let operations = vec![MergeOp::DeleteSymlink { path: "/link".to_string() }];

  apply_merge_operations(&engine, &context, &operations).unwrap();

  // Verify symlink is gone
  let head = engine.head_hash().unwrap();
  let tree = aeordb::engine::tree_walker::walk_version_tree(&engine, &head).unwrap();
  assert!(!tree.symlinks.contains_key("/link"), "symlink should be deleted");
}

#[test]
fn test_apply_missing_chunk_fails() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();

  // Create a FileRecord referencing a chunk that does not exist
  let fake_chunk_hash = vec![
    0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x01, 0x02,
    0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C,
  ];

  let file_record = FileRecord {
    path: "/broken.txt".to_string(),
    content_type: Some("text/plain".to_string()),
    total_size: 100,
    created_at: 1000,
    updated_at: 1000,
    metadata: Vec::new(),
    content_hash: vec![0x42; 32],
    chunk_hashes: vec![fake_chunk_hash],
  };

  let file_hash = aeordb::engine::file_identity_hash(
    &file_record.path,
    file_record.content_type.as_deref(),
    &file_record.chunk_hashes,
    &engine.hash_algo(),
  )
  .unwrap();
  let operations = vec![MergeOp::AddFile { path: "/broken.txt".to_string(), file_hash, file_record }];

  let result = apply_merge_operations(&engine, &context, &operations);
  assert!(result.is_err(), "should fail when chunk is missing");

  let error_message = format!("{}", result.unwrap_err());
  assert!(error_message.contains("Missing chunk"), "error should mention missing chunk, got: {}", error_message,);
}

#[test]
fn test_apply_multiple_operations_atomically() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();

  // Store two files first
  let (_, mut file_record_a) = store_and_get_record(&engine, "/a.txt", b"content a");
  let (_file_hash_b, _file_record_b) = store_and_get_record(&engine, "/b.txt", b"content b");

  let ops = DirectoryOps::new(&engine);
  ops.store_symlink(&context, "/old_link", "/nowhere").unwrap();

  // Apply multiple operations: add a file, delete a file, add a symlink, delete a symlink
  let new_link_record =
    SymlinkRecord { path: "/new_link".to_string(), target: "/new_from_a.txt".to_string(), created_at: 1000, updated_at: 1000 };
  let new_link_hash = aeordb::engine::symlink_identity_hash(&new_link_record.path, &new_link_record.target, &engine.hash_algo()).unwrap();
  file_record_a.path = "/new_from_a.txt".to_string();
  let file_hash_a = aeordb::engine::file_identity_hash(
    &file_record_a.path,
    file_record_a.content_type.as_deref(),
    &file_record_a.chunk_hashes,
    &engine.hash_algo(),
  )
  .unwrap();
  let operations = vec![
    MergeOp::AddFile { path: "/new_from_a.txt".to_string(), file_hash: file_hash_a, file_record: file_record_a },
    MergeOp::AddSymlink { path: "/new_link".to_string(), symlink_hash: new_link_hash, symlink_record: new_link_record },
    MergeOp::DeleteFile { path: "/b.txt".to_string() },
    MergeOp::DeleteSymlink { path: "/old_link".to_string() },
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

  let operations = vec![MergeOp::DeleteSymlink { path: "/ghost_link".to_string() }];

  let result = apply_merge_operations(&engine, &context, &operations);
  assert!(result.is_ok(), "deleting nonexistent symlink should not fail");
}

#[test]
fn sync_apply_rejects_cross_type_deletes_instead_of_treating_them_as_missing() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&context, "/cross-type-file", b"file", Some("text/plain")).unwrap();
  ops.store_symlink(&context, "/cross-type-link", "/target").unwrap();
  let before = engine.head_hash().unwrap();

  let file_error = apply_merge_operations(&engine, &context, &[MergeOp::DeleteFile { path: "/cross-type-link".to_string() }]).unwrap_err();
  assert!(matches!(file_error, EngineError::InvalidInput(_)), "unexpected error: {file_error}");
  let symlink_error =
    apply_merge_operations(&engine, &context, &[MergeOp::DeleteSymlink { path: "/cross-type-file".to_string() }]).unwrap_err();
  assert!(matches!(symlink_error, EngineError::InvalidInput(_)), "unexpected error: {symlink_error}");

  assert_eq!(engine.head_hash().unwrap(), before);
  assert_eq!(ops.read_file_buffered("/cross-type-file").unwrap(), b"file");
  assert!(ops.get_symlink("/cross-type-link").unwrap().is_some());
}

#[test]
fn test_apply_empty_operations() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();

  let result = apply_merge_operations(&engine, &context, &[]);
  assert!(result.is_ok(), "empty operations should succeed");
}

#[test]
fn sync_apply_failure_does_not_publish_earlier_operations() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let (_source_hash, mut file_record) = store_and_get_record(&engine, "/source.txt", b"atomic payload");
  file_record.path = "/would-have-been-created.txt".to_string();
  let file_hash = aeordb::engine::file_identity_hash(
    &file_record.path,
    file_record.content_type.as_deref(),
    &file_record.chunk_hashes,
    &engine.hash_algo(),
  )
  .unwrap();
  let before = engine.head_hash().unwrap();

  let operations = vec![
    MergeOp::AddFile { path: file_record.path.clone(), file_hash, file_record },
    MergeOp::AddSymlink {
      path: "/invalid-self-link".to_string(),
      symlink_hash: aeordb::engine::symlink_identity_hash("/invalid-self-link", "/invalid-self-link", &engine.hash_algo()).unwrap(),
      symlink_record: SymlinkRecord {
        path: "/invalid-self-link".to_string(),
        target: "/invalid-self-link".to_string(),
        created_at: 1,
        updated_at: 1,
      },
    },
  ];

  let error = apply_merge_operations(&engine, &context, &operations).unwrap_err();

  assert!(format!("{error}").contains("itself"), "unexpected error: {error}");
  assert_eq!(engine.head_hash().unwrap(), before, "failed sync apply must not publish a partial HEAD");
  assert!(DirectoryOps::new(&engine).read_file_buffered("/would-have-been-created.txt").is_err());
}

#[test]
fn sync_apply_propagates_corrupt_delete_instead_of_treating_it_as_not_found() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let (selected_hash, _record) = store_and_get_record(&engine, "/corrupt-delete.txt", b"retain me");
  let before = engine.head_hash().unwrap();
  engine.store_entry(aeordb::engine::EntryType::DirectoryIndex, &selected_hash, b"").unwrap();

  let error = apply_merge_operations(&engine, &context, &[MergeOp::DeleteFile { path: "/corrupt-delete.txt".to_string() }]).unwrap_err();

  assert!(matches!(error, EngineError::CorruptEntry { .. }), "unexpected error: {error}");
  assert_eq!(engine.head_hash().unwrap(), before, "failed delete must not move HEAD");
}

#[test]
fn sync_apply_propagates_corrupt_symlink_delete_instead_of_treating_it_as_not_found() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_symlink(&context, "/corrupt-delete-link", "/target").unwrap();
  let tree = aeordb::engine::tree_walker::walk_version_tree(&engine, &engine.head_hash().unwrap()).unwrap();
  let selected_hash = tree.symlinks["/corrupt-delete-link"].0.clone();
  let before = engine.head_hash().unwrap();
  engine.store_entry(aeordb::engine::EntryType::Symlink, &selected_hash, b"malformed symlink").unwrap();

  let error =
    apply_merge_operations(&engine, &context, &[MergeOp::DeleteSymlink { path: "/corrupt-delete-link".to_string() }]).unwrap_err();

  assert!(matches!(error, EngineError::CorruptEntry { .. }), "unexpected error: {error}");
  assert_eq!(engine.head_hash().unwrap(), before, "failed symlink delete must not move HEAD");
}

#[test]
fn sync_apply_memory_pressure_fails_before_any_authoritative_mutation() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let head_before = engine.head_hash().unwrap();
  let entries_before = engine.kv_entry_count().unwrap();
  let counters_before = engine.counters().snapshot();
  let durability_before = engine.durability_snapshot().unwrap();
  let coordinator = engine.memory_coordinator();
  let snapshot = coordinator.snapshot().unwrap();
  let policy = snapshot.policy.unwrap();
  let durability_owner_before = snapshot.owner(MemoryOwner::DurabilityWaiters).unwrap().clone();
  let remaining = policy.ordinary_limit_bytes().saturating_sub(snapshot.accounted_bytes);
  let _pressure = coordinator.reserve(MemoryOwner::Task, remaining.saturating_sub(4 * 1024), AdmissionClass::Workload).unwrap();
  let chunk_hashes = vec![vec![0x55; engine.hash_algo().hash_length()]; 256];
  let file_record = FileRecord {
    path: "/pressure.txt".to_string(),
    content_type: Some("text/plain".to_string()),
    total_size: 256,
    created_at: 1,
    updated_at: 1,
    metadata: Vec::new(),
    content_hash: vec![0x66; engine.hash_algo().hash_length()],
    chunk_hashes,
  };
  let file_hash = aeordb::engine::file_identity_hash(
    &file_record.path,
    file_record.content_type.as_deref(),
    &file_record.chunk_hashes,
    &engine.hash_algo(),
  )
  .unwrap();

  let error =
    apply_merge_operations(&engine, &context, &[MergeOp::AddFile { path: file_record.path.clone(), file_hash, file_record }]).unwrap_err();

  assert!(matches!(error, EngineError::ResourceExhausted(_)), "unexpected error: {error}");
  assert_eq!(engine.head_hash().unwrap(), head_before);
  assert_eq!(engine.kv_entry_count().unwrap(), entries_before);
  assert_eq!(engine.counters().snapshot().writes_total, counters_before.writes_total);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, durability_before.next_sequence);
  let owner = coordinator.snapshot().unwrap().owner(MemoryOwner::DurabilityWaiters).unwrap().clone();
  assert_eq!(owner.reserved_bytes, durability_owner_before.reserved_bytes);
  assert_eq!(owner.active_reservations, durability_owner_before.active_reservations);
}

#[test]
fn peer_apply_rejects_every_omitted_mutation_shape_before_mutation() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&context, "/.aeordb-system/config/delete-file.json", b"keep file", Some("application/json")).unwrap();
  ops.store_symlink(&context, "/.aeordb-system/config/delete-link", "/target").unwrap();

  let omitted_file = FileRecord {
    path: "/.aeordb-system/config/add-file.json".to_string(),
    content_type: Some("application/json".to_string()),
    total_size: 1,
    created_at: 1,
    updated_at: 1,
    metadata: Vec::new(),
    content_hash: vec![0x11; 32],
    chunk_hashes: vec![vec![0x22; 32]],
  };
  let operations = vec![
    MergeOp::AddFile { path: "/.aeordb-system/config/add-file.json".to_string(), file_hash: vec![0x33; 32], file_record: omitted_file },
    MergeOp::DeleteFile { path: "/.aeordb-system/config/delete-file.json".to_string() },
    MergeOp::AddSymlink {
      path: "/.aeordb-system/config/add-link".to_string(),
      symlink_hash: vec![0x44; 32],
      symlink_record: SymlinkRecord {
        path: "/.aeordb-system/config/add-link".to_string(),
        target: "/target".to_string(),
        created_at: 1,
        updated_at: 1,
      },
    },
    MergeOp::DeleteSymlink { path: "/.aeordb-system/config/delete-link".to_string() },
  ];

  let error = apply_merge_operations(&engine, &context, &operations).unwrap_err();

  assert!(matches!(error, EngineError::SystemFamilyPolicy { code: "system_family_transfer_omitted", .. }), "unexpected error: {error}");
  assert_eq!(ops.read_file_buffered("/.aeordb-system/config/delete-file.json").unwrap(), b"keep file");
  let config_entries = ops.list_directory("/.aeordb-system/config").unwrap();
  assert!(config_entries.iter().any(|entry| entry.name == "delete-link"));
}

#[test]
fn peer_apply_rejects_structural_container_as_a_leaf_before_chunk_reads() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let context = RequestContext::system();
  let file_record = FileRecord {
    path: "/.aeordb-system".to_string(),
    content_type: Some("application/octet-stream".to_string()),
    total_size: 1,
    created_at: 1,
    updated_at: 1,
    metadata: Vec::new(),
    content_hash: vec![0x55; 32],
    chunk_hashes: vec![vec![0x66; 32]],
  };
  let operations = vec![MergeOp::AddFile { path: "/.aeordb-system".to_string(), file_hash: vec![0x77; 32], file_record }];

  let error = apply_merge_operations(&engine, &context, &operations).unwrap_err();

  assert!(matches!(error, EngineError::SystemFamilyPolicy { code: "system_family_structural_leaf", .. }), "unexpected error: {error}");
}
