use aeordb::engine::btree::{BTREE_CONVERSION_THRESHOLD, is_btree_format};
use aeordb::engine::directory_entry::ChildEntry;
use aeordb::engine::directory_ops::{DirectoryOps, directory_content_hash};
use aeordb::engine::errors::EngineError;
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::{BufferedFile, EntryType, RequestContext, StorageEngine};
use aeordb::server::legacy_v3_root_adapter::{LegacyV3ResolvedPathV1, LegacyV3SelectedRootAdapterV1};
use aeordb::server::root_api::{RequestedRootSelectorV1, RootApiErrorV1};

fn create_engine() -> (tempfile::TempDir, StorageEngine) {
  let directory = tempfile::tempdir().unwrap();
  let database_path = directory.path().join("legacy-root-adapter.aeordb");
  let engine = StorageEngine::create(database_path.to_str().unwrap()).unwrap();
  DirectoryOps::new(&engine).ensure_root_directory(&RequestContext::system()).unwrap();
  (directory, engine)
}

#[test]
fn selected_root_retains_list_file_symlink_body_and_hash_after_head_advances() {
  let (_directory, engine) = create_engine();
  let context = RequestContext::system();
  let operations = DirectoryOps::new(&engine);
  operations.store_file_buffered(&context, "/docs/report.txt", b"root-x", Some("text/plain")).unwrap();
  operations.store_symlink(&context, "/docs/latest", "/docs/report.txt").unwrap();

  let selected_root = engine.head_hash().unwrap();
  let selected = LegacyV3SelectedRootAdapterV1::resolve(&engine, &RequestedRootSelectorV1::CurrentHead).unwrap();
  let selected_file = selected.file("/docs/report.txt").unwrap();
  assert_eq!(selected.root().hash, hex::encode(&selected_root));
  assert_eq!(selected.root().state, "live");

  operations.store_file_buffered(&context, "/docs/report.txt", b"root-y", Some("text/plain")).unwrap();
  operations.store_file_buffered(&context, "/docs/new.txt", b"new", Some("text/plain")).unwrap();
  let current_file =
    LegacyV3SelectedRootAdapterV1::resolve(&engine, &RequestedRootSelectorV1::CurrentHead).unwrap().file("/docs/report.txt").unwrap();

  assert_eq!(selected.read_file_body("/docs/report.txt").unwrap(), b"root-x");
  assert_eq!(selected.symlink("/docs/latest").unwrap().record.target, "/docs/report.txt");
  let LegacyV3ResolvedPathV1::File(followed) = selected.follow_path("/docs/latest").unwrap() else {
    panic!("selected symlink must resolve to its historical file");
  };
  assert_eq!(followed.record_hash, selected_file.record_hash);

  let entries = selected.list_directory("/docs").unwrap();
  assert!(entries.iter().any(|entry| entry.path == "/docs/report.txt"));
  assert!(entries.iter().any(|entry| entry.path == "/docs/latest" && entry.symlink_target.as_deref() == Some("/docs/report.txt")));
  assert!(!entries.iter().any(|entry| entry.path == "/docs/new.txt"));

  assert_eq!(selected.file_by_hash(&selected_file.record_hash).unwrap().record.path, "/docs/report.txt");
  assert!(matches!(selected.file_by_hash(&current_file.record_hash), Err(EngineError::NotFound(_))));
}

#[test]
fn snapshot_explicit_and_version_selectors_resolve_exactly_without_head_fallback() {
  let (_directory, engine) = create_engine();
  let context = RequestContext::system();
  let operations = DirectoryOps::new(&engine);
  operations.store_file_buffered(&context, "/value.txt", b"before", None).unwrap();
  let snapshot = VersionManager::new(&engine).create_snapshot(&context, "before", Default::default()).unwrap();
  operations.store_file_buffered(&context, "/value.txt", b"after", None).unwrap();

  for selector in [
    RequestedRootSelectorV1::Snapshot("before".to_string()),
    RequestedRootSelectorV1::ExplicitRoot(snapshot.root_hash.clone()),
    RequestedRootSelectorV1::VersionRoot(snapshot.root_hash.clone()),
  ] {
    let selected = LegacyV3SelectedRootAdapterV1::resolve(&engine, &selector).unwrap();
    assert_eq!(selected.root().hash, hex::encode(&snapshot.root_hash));
    assert_eq!(selected.root().state, "retained");
    assert_eq!(selected.read_file_body("/value.txt").unwrap(), b"before");
  }

  let unavailable = vec![0xA5; engine.hash_algo().hash_length()];
  assert_eq!(
    LegacyV3SelectedRootAdapterV1::resolve(&engine, &RequestedRootSelectorV1::ExplicitRoot(unavailable)).unwrap_err(),
    RootApiErrorV1::HistoricalViewUnavailable,
  );
  let missing_snapshot =
    LegacyV3SelectedRootAdapterV1::resolve(&engine, &RequestedRootSelectorV1::Snapshot("missing".to_string())).unwrap_err();
  assert_eq!(missing_snapshot, RootApiErrorV1::InvalidNamespaceRoot);
  assert!(matches!(missing_snapshot.engine_source(), Some(EngineError::NotFound(_))));
}

#[test]
fn selected_root_lists_flat_and_btree_directories_without_mixing_successor_entries() {
  let (_directory, engine) = create_engine();
  let context = RequestContext::system();
  let operations = DirectoryOps::new(&engine);
  let files = (0..=BTREE_CONVERSION_THRESHOLD)
    .map(|index| BufferedFile {
      path: format!("/wide/file-{index:04}.txt"),
      data: format!("value-{index}").into_bytes(),
      content_type: Some("text/plain".to_string()),
    })
    .collect();
  operations.store_files_buffered_batch(&context, files).unwrap();
  let selected_root = engine.head_hash().unwrap();
  let selected = LegacyV3SelectedRootAdapterV1::resolve(&engine, &RequestedRootSelectorV1::ExplicitRoot(selected_root.clone())).unwrap();
  let wide_entry = selected.list_directory("/").unwrap().into_iter().find(|entry| entry.path == "/wide").unwrap();
  let (_, _, directory_data) = engine.get_entry_including_deleted(&wide_entry.record_hash).unwrap().unwrap();
  assert!(is_btree_format(&directory_data));
  operations.store_file_buffered(&context, "/wide/successor.txt", b"successor", None).unwrap();

  let entries = selected.list_directory("/wide").unwrap();
  assert_eq!(entries.len(), BTREE_CONVERSION_THRESHOLD + 1);
  assert!(entries.iter().any(|entry| entry.path == "/wide/file-0000.txt"));
  assert!(!entries.iter().any(|entry| entry.path == "/wide/successor.txt"));
}

#[test]
fn wrong_type_and_corrupt_roots_fail_closed() {
  let (_directory, engine) = create_engine();
  let wrong_type_root = vec![0x91; engine.hash_algo().hash_length()];
  engine.store_entry(EntryType::Chunk, &wrong_type_root, b"not-a-root").unwrap();
  assert_eq!(
    LegacyV3SelectedRootAdapterV1::resolve(&engine, &RequestedRootSelectorV1::ExplicitRoot(wrong_type_root)).unwrap_err(),
    RootApiErrorV1::InvalidNamespaceRoot,
  );

  let corrupt_root = vec![0x92; engine.hash_algo().hash_length()];
  engine.store_entry(EntryType::DirectoryIndex, &corrupt_root, &[0xFF]).unwrap();
  let selected = LegacyV3SelectedRootAdapterV1::resolve(&engine, &RequestedRootSelectorV1::ExplicitRoot(corrupt_root)).unwrap();
  assert!(matches!(selected.list_directory("/"), Err(EngineError::UnexpectedEof | EngineError::CorruptEntry { .. })));
}

#[test]
fn corrupt_directory_names_cannot_escape_the_selected_namespace_path() {
  let (_directory, engine) = create_engine();
  let context = RequestContext::system();
  let operations = DirectoryOps::new(&engine);
  operations.store_file_buffered(&context, "/safe.txt", b"safe", None).unwrap();
  let current = LegacyV3SelectedRootAdapterV1::resolve(&engine, &RequestedRootSelectorV1::CurrentHead).unwrap();
  let file = current.file("/safe.txt").unwrap();

  let child = ChildEntry {
    entry_type: EntryType::FileRecord.to_u8(),
    hash: file.record_hash,
    total_size: file.record.total_size,
    created_at: file.record.created_at,
    updated_at: file.record.updated_at,
    name: "../escape.txt".to_string(),
    content_type: None,
    virtual_time: 1,
    node_id: 1,
  };
  let directory_value = child.serialize(engine.hash_algo().hash_length()).unwrap();
  let corrupt_root = directory_content_hash(&directory_value, &engine.hash_algo()).unwrap();
  engine.store_entry(EntryType::DirectoryIndex, &corrupt_root, &directory_value).unwrap();
  let selected = LegacyV3SelectedRootAdapterV1::resolve(&engine, &RequestedRootSelectorV1::ExplicitRoot(corrupt_root)).unwrap();
  assert!(matches!(selected.list_directory("/"), Err(EngineError::CorruptEntry { .. })));
}

#[test]
fn corrupt_directory_file_hashes_cannot_alias_a_different_record_path() {
  let (_directory, engine) = create_engine();
  let context = RequestContext::system();
  let operations = DirectoryOps::new(&engine);
  operations.store_file_buffered(&context, "/safe.txt", b"safe", None).unwrap();
  let current = LegacyV3SelectedRootAdapterV1::resolve(&engine, &RequestedRootSelectorV1::CurrentHead).unwrap();
  let file = current.file("/safe.txt").unwrap();

  let child = ChildEntry {
    entry_type: EntryType::FileRecord.to_u8(),
    hash: file.record_hash,
    total_size: file.record.total_size,
    created_at: file.record.created_at,
    updated_at: file.record.updated_at,
    name: "alias.txt".to_string(),
    content_type: None,
    virtual_time: 1,
    node_id: 1,
  };
  let directory_value = child.serialize(engine.hash_algo().hash_length()).unwrap();
  let corrupt_root = directory_content_hash(&directory_value, &engine.hash_algo()).unwrap();
  engine.store_entry(EntryType::DirectoryIndex, &corrupt_root, &directory_value).unwrap();
  let selected = LegacyV3SelectedRootAdapterV1::resolve(&engine, &RequestedRootSelectorV1::ExplicitRoot(corrupt_root)).unwrap();
  assert!(matches!(selected.list_directory("/"), Err(EngineError::CorruptEntry { .. })));
}

#[test]
fn selected_root_symlink_cycles_and_dangling_targets_remain_typed_failures() {
  let (_directory, engine) = create_engine();
  let context = RequestContext::system();
  let operations = DirectoryOps::new(&engine);
  operations.store_symlink(&context, "/cycle-a", "/cycle-b").unwrap();
  operations.store_symlink(&context, "/cycle-b", "/cycle-a").unwrap();
  operations.store_symlink(&context, "/dangling", "/missing").unwrap();
  let selected = LegacyV3SelectedRootAdapterV1::resolve(&engine, &RequestedRootSelectorV1::CurrentHead).unwrap();

  assert!(matches!(selected.follow_path("/cycle-a"), Err(EngineError::CyclicSymlink(_))));
  assert!(matches!(selected.follow_path("/dangling"), Err(EngineError::NotFound(_))));
}

#[test]
fn compatibility_adapter_is_the_single_head_capture_and_legacy_tree_walk_owner() {
  let source =
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/legacy_v3_root_adapter.rs")).unwrap();
  assert_eq!(source.matches("engine.head_hash()").count(), 1);
  for forbidden in ["DirectoryOps", "resolve_root_hash", "get_metadata", "list_directory_strict", "read_file_buffered"] {
    assert!(!source.contains(forbidden), "compatibility adapter escaped to mutable authority through {forbidden}");
  }
  for required in [
    "resolve_directory_at_version",
    "resolve_file_at_version",
    "resolve_symlink_at_version",
    "from_chunk_hashes_including_deleted",
    "file_by_hash",
  ] {
    assert!(source.contains(required), "compatibility adapter lost required exact-root authority {required}");
  }
}
