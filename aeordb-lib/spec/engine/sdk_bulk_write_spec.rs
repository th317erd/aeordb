use std::fs::File;

use aeordb::engine::{
  apply_merge_patch, directory_content_hash, directory_path_hash, file_path_hash, BufferedFile, DirectoryOps, EngineError, EntryType,
  JsonMergeFilePatch, MergeDepth, RequestContext, StorageEngine, CURRENT_FILE_RECORD_VERSION,
};
use aeordb::engine::file_header::read_active_header;
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
use aeordb::engine::storage_engine::TransactionGuard;
use serde_json::json;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let path = dir.path().join("test.aeor");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory(&ctx).unwrap();
  engine
}

fn read_json(ops: &DirectoryOps<'_>, path: &str) -> serde_json::Value {
  let bytes = ops.read_file_buffered(path).expect("file should exist");
  serde_json::from_slice(&bytes).expect("stored content should be JSON")
}

fn disk_head_hash(dir: &tempfile::TempDir) -> Vec<u8> {
  let mut file = File::open(dir.path().join("test.aeor")).unwrap();
  let (header, _) = read_active_header(&mut file).unwrap();
  header.head_hash
}

fn invalid_message(result: Result<impl std::fmt::Debug, EngineError>) -> String {
  match result {
    Err(EngineError::InvalidInput(message)) => message,
    other => panic!("expected InvalidInput, got {other:?}"),
  }
}

#[test]
fn merge_patch_primitive_is_exported_from_engine() {
  let mut target = json!({
    "profile": {
      "name": "Ada",
      "prefs": {"theme": "dark", "density": "compact"}
    },
    "stale": true
  });

  apply_merge_patch(
    &mut target,
    json!({
      "profile": {"prefs": {"theme": "light"}},
      "stale": null
    }),
    MergeDepth::Unbounded,
  );

  assert_eq!(
    target,
    json!({
      "profile": {
        "name": "Ada",
        "prefs": {"theme": "light", "density": "compact"}
      }
    })
  );
}

#[test]
fn store_files_buffered_batch_stores_multiple_small_files() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let result = ops
    .store_files_buffered_batch(
      &ctx,
      vec![
        BufferedFile { path: "/bulk/a.txt".to_string(), data: b"alpha".to_vec(), content_type: Some("text/plain".to_string()) },
        BufferedFile {
          path: "/bulk/nested/b.json".to_string(),
          data: br#"{"beta":true}"#.to_vec(),
          content_type: Some("application/json".to_string()),
        },
        BufferedFile {
          path: "/bulk/nested/c.bin".to_string(),
          data: vec![0, 1, 2, 3],
          content_type: Some("application/octet-stream".to_string()),
        },
      ],
    )
    .unwrap();

  assert_eq!(result.committed, 3);
  assert_eq!(ops.read_file_buffered("/bulk/a.txt").unwrap(), b"alpha");
  assert_eq!(ops.read_file_buffered("/bulk/nested/b.json").unwrap(), br#"{"beta":true}"#);
  assert_eq!(ops.read_file_buffered("/bulk/nested/c.bin").unwrap(), vec![0, 1, 2, 3]);

  let metadata = ops.get_metadata("/bulk/nested/b.json").unwrap().unwrap();
  assert_eq!(metadata.total_size, br#"{"beta":true}"#.len() as u64);
  assert_eq!(metadata.content_type.as_deref(), Some("application/json"));
  assert_eq!(metadata.content_hash, blake3::hash(br#"{"beta":true}"#).as_bytes().to_vec());
  let file_key = file_path_hash("/bulk/nested/b.json", &engine.hash_algo()).unwrap();
  let (header, _key, _value) = engine.get_entry(&file_key).unwrap().unwrap();
  assert_eq!(header.entry_version, CURRENT_FILE_RECORD_VERSION);

  let children = ops.list_directory("/bulk").unwrap();
  let names: Vec<&str> = children.iter().map(|child| child.name.as_str()).collect();
  assert!(names.contains(&"a.txt"));
  assert!(names.contains(&"nested"));
}

#[test]
fn store_files_buffered_batch_writes_directory_path_keys_as_hard_links() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops
    .store_files_buffered_batch(
      &ctx,
      vec![
        BufferedFile {
          path: "/bulk/nested/a.json".to_string(),
          data: br#"{"a":1}"#.to_vec(),
          content_type: Some("application/json".to_string()),
        },
        BufferedFile {
          path: "/bulk/nested/b.json".to_string(),
          data: br#"{"b":2}"#.to_vec(),
          content_type: Some("application/json".to_string()),
        },
      ],
    )
    .unwrap();

  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();
  for path in ["/", "/bulk", "/bulk/nested"] {
    let dir_key = directory_path_hash(path, &algo).unwrap();
    let (_header, _key, value) = engine.get_entry(&dir_key).unwrap().expect("directory path key should exist");
    assert_eq!(value.len(), hash_length, "{} should store a content-hash hard link", path);
    assert!(engine.has_entry(&value).unwrap(), "{} hard-link target should exist", path);
  }

  let nested = ops.list_directory("/bulk/nested").unwrap();
  let names: Vec<&str> = nested.iter().map(|child| child.name.as_str()).collect();
  assert!(names.contains(&"a.json"));
  assert!(names.contains(&"b.json"));
}

#[test]
fn store_files_buffered_batch_publishes_hot_tail_after_large_transaction() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let files: Vec<BufferedFile> = (0..160)
    .map(|i| BufferedFile {
      path: format!("/large-batch/file-{i:04}.json"),
      data: format!(r#"{{"i":{i}}}"#).into_bytes(),
      content_type: Some("application/json".to_string()),
    })
    .collect();

  ops.store_files_buffered_batch(&ctx, files).unwrap();

  let writer = engine.writer_read_lock().unwrap();
  let header = writer.file_header().clone();
  assert_eq!(
    header.hot_tail_offset,
    writer.current_offset(),
    "batch commit must publish the current WAL end even if the hot buffer flushed during the transaction"
  );
}

#[test]
fn store_files_buffered_batch_builds_and_updates_btree_directories_that_survive_reopen() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("test.aeor");

  {
    let engine = create_engine(&dir);
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    let files = (0..300)
      .map(|index| BufferedFile {
        path: format!("/btree-batch/file-{index:04}.txt"),
        data: format!("initial-{index}").into_bytes(),
        content_type: Some("text/plain".to_string()),
      })
      .collect();
    ops.store_files_buffered_batch(&ctx, files).unwrap();

    let directory_key = directory_path_hash("/btree-batch", &engine.hash_algo()).unwrap();
    let (_path_header, _path_key, content_key) = engine.get_entry(&directory_key).unwrap().unwrap();
    let (_content_header, _content_key, directory_data) = engine.get_entry(&content_key).unwrap().unwrap();
    assert!(aeordb::engine::is_btree_format(&directory_data), "a newly published 300-child directory must use the B-tree format");

    ops
      .store_files_buffered_batch(
        &ctx,
        vec![
          BufferedFile {
            path: "/btree-batch/file-0007.txt".to_string(),
            data: b"updated-seven".to_vec(),
            content_type: Some("text/plain".to_string()),
          },
          BufferedFile {
            path: "/btree-batch/file-0300.txt".to_string(),
            data: b"new-three-hundred".to_vec(),
            content_type: Some("text/plain".to_string()),
          },
        ],
      )
      .unwrap();

    let listing = ops.list_directory_strict("/btree-batch").unwrap();
    assert_eq!(listing.len(), 301);
    assert_eq!(listing.first().unwrap().name, "file-0000.txt");
    assert_eq!(listing.last().unwrap().name, "file-0300.txt");
    assert_eq!(ops.read_file_buffered("/btree-batch/file-0007.txt").unwrap(), b"updated-seven");
  }

  let reopened = StorageEngine::open(path.to_str().unwrap()).unwrap();
  let reopened_ops = DirectoryOps::new(&reopened);
  let listing = reopened_ops.list_directory_strict("/btree-batch").unwrap();
  assert_eq!(listing.len(), 301);
  assert_eq!(reopened_ops.read_file_buffered("/btree-batch/file-0300.txt").unwrap(), b"new-three-hundred");
}

#[test]
fn store_file_buffered_rejects_legacy_outer_transaction_without_publishing_head() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let initial_disk_head = disk_head_hash(&dir);
  let initial_memory_head = engine.head_hash().unwrap();

  {
    let _outer = TransactionGuard::new(&engine).unwrap();
    let message = invalid_message(ops.store_file_buffered(&ctx, "/txn/a.json", br#"{"a":1}"#, Some("application/json")));
    assert!(message.contains("top-level namespace mutation"));
    assert!(ops.get_metadata("/txn/a.json").unwrap().is_none());
    assert_eq!(engine.head_hash().unwrap(), initial_memory_head);

    engine.try_flush_hot_buffer();
    assert_eq!(disk_head_hash(&dir), initial_disk_head, "a refused namespace mutation must not publish HEAD through the outer transaction");
  }

  assert_eq!(disk_head_hash(&dir), initial_disk_head);
  assert_eq!(engine.head_hash().unwrap(), initial_memory_head);
  assert!(ops.read_file_buffered("/txn/a.json").is_err());
}

#[test]
fn store_files_buffered_batch_rejects_legacy_outer_transaction_without_publishing_head() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let initial_disk_head = disk_head_hash(&dir);
  let initial_memory_head = engine.head_hash().unwrap();

  {
    let _outer = TransactionGuard::new(&engine).unwrap();
    let message = invalid_message(ops.store_files_buffered_batch(
      &ctx,
      vec![
        BufferedFile {
          path: "/txn/batch/a.json".to_string(),
          data: br#"{"a":1}"#.to_vec(),
          content_type: Some("application/json".to_string()),
        },
        BufferedFile {
          path: "/txn/batch/b.json".to_string(),
          data: br#"{"b":2}"#.to_vec(),
          content_type: Some("application/json".to_string()),
        },
      ],
    ));
    assert!(message.contains("top-level namespace mutation"));
    assert_eq!(engine.head_hash().unwrap(), initial_memory_head);
    assert!(ops.get_metadata("/txn/batch/a.json").unwrap().is_none());
    assert!(ops.get_metadata("/txn/batch/b.json").unwrap().is_none());

    engine.try_flush_hot_buffer();
    assert_eq!(disk_head_hash(&dir), initial_disk_head, "a refused batch must not publish HEAD through the outer transaction");
  }

  assert_eq!(disk_head_hash(&dir), initial_disk_head);
  assert_eq!(engine.head_hash().unwrap(), initial_memory_head);
  assert!(ops.read_file_buffered("/txn/batch/a.json").is_err());
  assert!(ops.read_file_buffered("/txn/batch/b.json").is_err());
}

#[test]
fn copy_path_recursively_copies_files_empty_directories_and_symlinks() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/copy-source/file.txt", b"payload", Some("text/plain")).unwrap();
  ops.create_directory(&ctx, "/copy-source/empty").unwrap();
  ops.store_symlink(&ctx, "/copy-source/link", "/copy-source/file.txt").unwrap();

  let mut copied = ops.copy_path(&ctx, "/copy-source", "/copy-destination").unwrap();
  copied.sort();
  assert_eq!(copied, vec!["/copy-destination/file.txt", "/copy-destination/link"]);
  assert_eq!(ops.read_file_buffered("/copy-destination/file.txt").unwrap(), b"payload");
  assert!(ops.list_directory_strict("/copy-destination/empty").unwrap().is_empty());
  let copied_link = ops.get_symlink("/copy-destination/link").unwrap().expect("copied symlink should exist");
  assert_eq!(copied_link.target, "/copy-source/file.txt");
}

#[test]
fn copy_file_rejects_non_file_sources_before_publishing() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.create_directory(&ctx, "/copy-file-source-directory").unwrap();
  ops.store_file_buffered(&ctx, "/copy-file-target.txt", b"target", Some("text/plain")).unwrap();
  ops.store_symlink(&ctx, "/copy-file-source-link", "/copy-file-target.txt").unwrap();
  let original_head = engine.head_hash().unwrap();

  for (source, destination) in
    [("/copy-file-source-directory", "/copied-as-file-directory"), ("/copy-file-source-link", "/copied-as-file-link")]
  {
    let message = invalid_message(ops.copy_file(&ctx, source, destination));
    assert!(message.contains("file source"), "unexpected copy_file type error: {message}");
    assert_eq!(engine.head_hash().unwrap(), original_head, "copy_file type rejection must precede HEAD publication");
    assert!(ops.get_metadata(destination).unwrap().is_none());
    assert!(ops.get_symlink(destination).unwrap().is_none());
    assert!(ops.list_directory_strict(destination).is_err());
  }
}

#[test]
fn copy_paths_rejects_self_descendants_before_publishing_any_destination() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&ctx, "/source/file.txt", b"payload", Some("text/plain")).unwrap();
  let original_head = engine.head_hash().unwrap();

  let message = invalid_message(ops.copy_paths(&ctx, &["/source".to_string()], "/source/nested"));
  assert!(message.contains("descendant"));
  assert_eq!(engine.head_hash().unwrap(), original_head);
  assert!(ops.get_metadata("/source/nested/source/file.txt").unwrap().is_none());
}

#[test]
fn copy_planning_memory_refusal_precedes_namespace_publication() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&ctx, "/source/file.txt", b"bounded copy planning", Some("text/plain")).unwrap();
  let original_head = engine.head_hash().unwrap();

  let coordinator = engine.memory_coordinator();
  let before = coordinator.snapshot().unwrap();
  let policy = before.policy.unwrap();
  let available = policy.ordinary_limit_bytes().saturating_sub(before.accounted_bytes);
  assert!(available > 64, "test requires ordinary memory headroom");
  let pressure = coordinator.reserve(MemoryOwner::Query, available - 64, AdmissionClass::Workload).unwrap();

  let result = ops.copy_path(&ctx, "/source", "/destination");
  assert!(matches!(result, Err(EngineError::ResourceExhausted(_))), "copy planning must honor the process memory envelope: {result:?}");
  assert_eq!(engine.head_hash().unwrap(), original_head);
  assert!(ops.list_directory_strict("/destination").is_err());

  drop(pressure);
  let after = coordinator.snapshot().unwrap();
  let owner = after.owner(MemoryOwner::DurabilityWaiters).unwrap();
  assert_eq!(owner.reserved_bytes, before.owner(MemoryOwner::DurabilityWaiters).unwrap().reserved_bytes);
  assert_eq!(owner.active_reservations, before.owner(MemoryOwner::DurabilityWaiters).unwrap().active_reservations);
}

#[test]
fn failed_recursive_copy_does_not_heal_source_locators_during_planning() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&ctx, "/source/file.txt", b"source", Some("text/plain")).unwrap();
  poison_directory_path_key_with_empty_hard_link(&engine, "/source");

  let source_key = directory_path_hash("/source", &engine.hash_algo()).unwrap();
  let stale_target = engine.get_entry(&source_key).unwrap().unwrap().2;
  let original_head = engine.head_hash().unwrap();
  let result = ops.copy_paths(&ctx, &["/missing".to_string(), "/source".to_string()], "/destination");

  assert!(matches!(result, Err(EngineError::NotFound(path)) if path == "/missing"));
  assert_eq!(engine.get_entry(&source_key).unwrap().unwrap().2, stale_target, "validation must not perform a hidden source repair write");
  assert_eq!(engine.head_hash().unwrap(), original_head);
  assert!(ops.get_metadata("/destination/source/file.txt").unwrap().is_none());
}

#[test]
fn copy_and_rename_upgrade_legacy_v0_file_records_with_exact_content_hashes() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let content: Vec<u8> = (0..300_123).map(|index| (index % 251) as u8).collect();

  ops.store_file_buffered(&ctx, "/legacy/copy.bin", &content, Some("application/octet-stream")).unwrap();
  let copy_hash = rewrite_file_record_path_as_v0(&engine, "/legacy/copy.bin");
  ops.copy_file(&ctx, "/legacy/copy.bin", "/current/copied.bin").unwrap();
  let copied = ops.get_metadata("/current/copied.bin").unwrap().unwrap();
  assert_eq!(copied.content_hash, copy_hash);
  assert_eq!(ops.read_file_buffered("/current/copied.bin").unwrap(), content);
  let copied_key = file_path_hash("/current/copied.bin", &engine.hash_algo()).unwrap();
  assert_eq!(engine.get_entry(&copied_key).unwrap().unwrap().0.entry_version, CURRENT_FILE_RECORD_VERSION);

  ops.store_file_buffered(&ctx, "/legacy/rename.bin", &content, Some("application/octet-stream")).unwrap();
  let rename_hash = rewrite_file_record_path_as_v0(&engine, "/legacy/rename.bin");
  let renamed = ops.rename_file(&ctx, "/legacy/rename.bin", "/current/renamed.bin").unwrap();
  assert_eq!(renamed.content_hash, rename_hash);
  assert_eq!(ops.read_file_buffered("/current/renamed.bin").unwrap(), content);
  assert!(ops.get_metadata("/legacy/rename.bin").unwrap().is_none());
  let renamed_key = file_path_hash("/current/renamed.bin", &engine.hash_algo()).unwrap();
  assert_eq!(engine.get_entry(&renamed_key).unwrap().unwrap().0.entry_version, CURRENT_FILE_RECORD_VERSION);
}

#[test]
fn rename_rejects_source_records_with_missing_chunks_before_publishing() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&ctx, "/rename-corrupt-source.bin", b"payload", Some("application/octet-stream")).unwrap();
  let source = ops.get_metadata("/rename-corrupt-source.bin").unwrap().unwrap();
  engine.mark_entry_deleted(&source.chunk_hashes[0]).unwrap();
  let original_head = engine.head_hash().unwrap();

  let result = ops.rename_file(&ctx, "/rename-corrupt-source.bin", "/renamed-corrupt-source.bin");

  assert!(matches!(result, Err(EngineError::CorruptEntry { reason, .. }) if reason.contains("missing chunk")));
  assert_eq!(engine.head_hash().unwrap(), original_head);
  assert!(ops.get_metadata("/rename-corrupt-source.bin").unwrap().is_some());
  assert!(ops.get_metadata("/renamed-corrupt-source.bin").unwrap().is_none());
}

#[test]
fn legacy_copy_content_hash_backfill_obeys_planning_memory_admission() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let content = vec![0x5a; 300_123];
  ops.store_file_buffered(&ctx, "/legacy.bin", &content, Some("application/octet-stream")).unwrap();
  rewrite_file_record_path_as_v0(&engine, "/legacy.bin");
  let original_head = engine.head_hash().unwrap();

  let coordinator = engine.memory_coordinator();
  let before = coordinator.snapshot().unwrap();
  let available = before.policy.unwrap().ordinary_limit_bytes().saturating_sub(before.accounted_bytes);
  let remaining = 128 * 1024;
  assert!(available > remaining, "test requires ordinary memory headroom");
  let pressure = coordinator.reserve(MemoryOwner::Query, available - remaining, AdmissionClass::Workload).unwrap();

  let result = ops.copy_file(&ctx, "/legacy.bin", "/copied.bin");
  assert!(matches!(result, Err(EngineError::ResourceExhausted(_))), "legacy content-hash backfill bypassed copy admission: {result:?}");
  assert_eq!(engine.head_hash().unwrap(), original_head);
  assert!(ops.get_metadata("/copied.bin").unwrap().is_none());

  drop(pressure);
}

#[test]
fn batch_and_copy_reject_non_directory_ancestors_without_publishing() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&ctx, "/blocked-file", b"file", Some("text/plain")).unwrap();
  ops.store_symlink(&ctx, "/blocked-link", "/blocked-file").unwrap();
  ops.store_file_buffered(&ctx, "/copy-source.txt", b"source", Some("text/plain")).unwrap();
  let original_head = engine.head_hash().unwrap();

  for blocked_path in ["/blocked-file/child.txt", "/blocked-link/child.txt"] {
    let result = ops.store_files_buffered_batch(
      &ctx,
      vec![BufferedFile { path: blocked_path.to_string(), data: b"child".to_vec(), content_type: Some("text/plain".to_string()) }],
    );
    assert!(matches!(result, Err(EngineError::AlreadyExists(_))), "non-directory ancestor should reject {blocked_path}: {result:?}");
    assert!(ops.get_metadata(blocked_path).unwrap().is_none());
  }

  let copy_result = ops.copy_paths(&ctx, &["/copy-source.txt".to_string()], "/blocked-file");
  assert!(matches!(copy_result, Err(EngineError::AlreadyExists(_))), "copy destination file must not become an implicit directory");
  assert!(ops.get_metadata("/blocked-file/copy-source.txt").unwrap().is_none());
  assert_eq!(engine.head_hash().unwrap(), original_head);
}

#[test]
fn store_file_buffered_merges_against_head_when_dir_path_key_is_stale() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/stale/a.txt", b"a", Some("text/plain")).unwrap();
  poison_directory_path_key_with_empty_hard_link(&engine, "/stale");

  ops.store_file_buffered(&ctx, "/stale/b.txt", b"b", Some("text/plain")).unwrap();

  let children = ops.list_directory("/stale").unwrap();
  let names: Vec<&str> = children.iter().map(|child| child.name.as_str()).collect();
  assert!(names.contains(&"a.txt"), "existing HEAD child should survive stale dir_key mutation");
  assert!(names.contains(&"b.txt"));
}

#[test]
fn store_files_buffered_batch_merges_against_head_when_dir_path_key_is_stale() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/stale/a.txt", b"a", Some("text/plain")).unwrap();
  poison_directory_path_key_with_empty_hard_link(&engine, "/stale");

  ops
    .store_files_buffered_batch(
      &ctx,
      vec![BufferedFile { path: "/stale/c.txt".to_string(), data: b"c".to_vec(), content_type: Some("text/plain".to_string()) }],
    )
    .unwrap();

  let children = ops.list_directory("/stale").unwrap();
  let names: Vec<&str> = children.iter().map(|child| child.name.as_str()).collect();
  assert!(names.contains(&"a.txt"), "existing HEAD child should survive stale batch dir_key mutation");
  assert!(names.contains(&"c.txt"));
}

fn poison_directory_path_key_with_empty_hard_link(engine: &StorageEngine, path: &str) {
  let algo = engine.hash_algo();
  let empty_dir = Vec::new();
  let empty_content_key = directory_content_hash(&empty_dir, &algo).unwrap();
  engine.store_entry(EntryType::DirectoryIndex, &empty_content_key, &empty_dir).unwrap();
  let dir_key = directory_path_hash(path, &algo).unwrap();
  engine.store_entry(EntryType::DirectoryIndex, &dir_key, &empty_content_key).unwrap();
}

fn rewrite_file_record_path_as_v0(engine: &StorageEngine, path: &str) -> Vec<u8> {
  let record = DirectoryOps::new(engine).get_metadata(path).unwrap().unwrap();
  let content_hash = record.content_hash.clone();
  let value = record.serialize_for_version(engine.hash_algo().hash_length(), 0).unwrap();
  let path_key = file_path_hash(path, &engine.hash_algo()).unwrap();
  engine.store_entry_with_version(EntryType::FileRecord, &path_key, &value, 0).unwrap();
  content_hash
}

#[test]
fn store_files_buffered_batch_rejects_invalid_batches_before_writing() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let empty_message = invalid_message(ops.store_files_buffered_batch(&ctx, Vec::new()));
  assert!(empty_message.contains("No files"));

  let root_message = invalid_message(
    ops.store_files_buffered_batch(&ctx, vec![BufferedFile { path: "/".to_string(), data: b"bad".to_vec(), content_type: None }]),
  );
  assert!(root_message.contains("Cannot store at root path"));

  let duplicate_message = invalid_message(ops.store_files_buffered_batch(
    &ctx,
    vec![
      BufferedFile { path: "/dup/a.txt".to_string(), data: b"one".to_vec(), content_type: None },
      BufferedFile { path: "dup/a.txt".to_string(), data: b"two".to_vec(), content_type: None },
    ],
  ));
  assert!(duplicate_message.contains("Duplicate batch path"));
  assert!(ops.read_file_buffered("/dup/a.txt").is_err());
}

#[test]
fn store_files_buffered_batch_preserves_created_at_on_overwrite() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops
    .store_files_buffered_batch(
      &ctx,
      vec![BufferedFile { path: "/overwrite/doc.txt".to_string(), data: b"one".to_vec(), content_type: Some("text/plain".to_string()) }],
    )
    .unwrap();
  let first = ops.get_metadata("/overwrite/doc.txt").unwrap().unwrap();

  ops
    .store_files_buffered_batch(
      &ctx,
      vec![BufferedFile { path: "/overwrite/doc.txt".to_string(), data: b"two".to_vec(), content_type: Some("text/plain".to_string()) }],
    )
    .unwrap();
  let second = ops.get_metadata("/overwrite/doc.txt").unwrap().unwrap();

  assert_eq!(first.created_at, second.created_at);
  assert_eq!(ops.read_file_buffered("/overwrite/doc.txt").unwrap(), b"two");
}

#[test]
fn store_files_buffered_batch_supports_embedded_system_paths() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops
    .store_files_buffered_batch(
      &ctx,
      vec![BufferedFile {
        path: "/.aeordb-system/sync/state.json".to_string(),
        data: br#"{"checkpoint":42}"#.to_vec(),
        content_type: Some("application/json".to_string()),
      }],
    )
    .unwrap();

  assert_eq!(ops.read_file_buffered("/.aeordb-system/sync/state.json").unwrap(), br#"{"checkpoint":42}"#);
}

#[test]
fn merge_json_file_creates_and_updates_documents() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let created = ops.merge_json_file(&ctx, "/state/doc.json", json!({"a": 1, "nested": {"x": 1}}), MergeDepth::Unbounded).unwrap();
  assert!(created.created);
  assert_eq!(created.file_record.content_type.as_deref(), Some("application/json"));

  let updated = ops.merge_json_file(&ctx, "/state/doc.json", json!({"a": null, "nested": {"y": 2}}), MergeDepth::Unbounded).unwrap();
  assert!(!updated.created);

  assert_eq!(read_json(&ops, "/state/doc.json"), json!({"nested": {"x": 1, "y": 2}}));
}

#[test]
fn merge_json_file_rejects_a_non_chunk_entry_at_a_derived_chunk_key() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let patch = json!({"value": "collision fixture"});
  let serialized = serde_json::to_vec(&patch).unwrap();
  let chunk_key = aeordb::engine::chunk_content_hash(&serialized, &engine.hash_algo()).unwrap();
  engine.store_entry(EntryType::DirectoryIndex, &chunk_key, b"wrong entry type").unwrap();
  let original_head = engine.head_hash().unwrap();

  let result = ops.merge_json_file(&ctx, "/state/collision.json", patch, MergeDepth::Unbounded);

  assert!(matches!(result, Err(EngineError::CorruptEntry { reason, .. }) if reason.contains("non-chunk")));
  assert_eq!(engine.head_hash().unwrap(), original_head);
  assert!(ops.get_metadata("/state/collision.json").unwrap().is_none());
}

#[test]
fn merge_json_file_honors_depth_and_rejects_invalid_existing_json() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops
    .store_file_buffered(&ctx, "/state/depth.json", br#"{"outer":{"keep":"yes","inner":{"x":1,"z":0}}}"#, Some("application/json"))
    .unwrap();
  ops.merge_json_file(&ctx, "/state/depth.json", json!({"outer": {"inner": {"x": 2}}}), MergeDepth::ReplaceBeyond(2)).unwrap();
  assert_eq!(read_json(&ops, "/state/depth.json"), json!({"outer": {"keep": "yes", "inner": {"x": 2}}}));

  ops.store_file_buffered(&ctx, "/state/bad.json", b"not json", Some("text/plain")).unwrap();
  let message = invalid_message(ops.merge_json_file(&ctx, "/state/bad.json", json!({"a": 1}), MergeDepth::Unbounded));
  assert!(message.contains("not valid JSON"));
}

#[test]
fn merge_json_files_batch_merges_many_and_preserves_atomicity_on_read_failures() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/state/a.json", br#"{"a":1,"nested":{"x":1}}"#, Some("application/json")).unwrap();

  let result = ops
    .merge_json_files_batch(
      &ctx,
      vec![
        JsonMergeFilePatch { path: "/state/a.json".to_string(), patch: json!({"nested": {"y": 2}}), depth: MergeDepth::Unbounded },
        JsonMergeFilePatch { path: "/state/b.json".to_string(), patch: json!({"b": 2}), depth: MergeDepth::Unbounded },
      ],
    )
    .unwrap();

  assert_eq!(result.merged, 2);
  assert!(!result.files.iter().find(|file| file.path == "/state/a.json").unwrap().created);
  assert!(result.files.iter().find(|file| file.path == "/state/b.json").unwrap().created);
  assert_eq!(read_json(&ops, "/state/a.json"), json!({"a": 1, "nested": {"x": 1, "y": 2}}));
  assert_eq!(read_json(&ops, "/state/b.json"), json!({"b": 2}));

  ops.store_file_buffered(&ctx, "/state/bad.json", b"bad json", Some("application/json")).unwrap();
  let message = invalid_message(ops.merge_json_files_batch(
    &ctx,
    vec![
      JsonMergeFilePatch { path: "/state/new.json".to_string(), patch: json!({"new": true}), depth: MergeDepth::Unbounded },
      JsonMergeFilePatch { path: "/state/bad.json".to_string(), patch: json!({"never": "written"}), depth: MergeDepth::Unbounded },
    ],
  ));
  assert!(message.contains("not valid JSON"));
  assert!(ops.read_file_buffered("/state/new.json").is_err());
}

#[test]
fn merge_json_files_batch_rejects_invalid_batch_shapes_before_writing() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let empty_message = invalid_message(ops.merge_json_files_batch(&ctx, Vec::new()));
  assert!(empty_message.contains("No JSON merge patches"));

  let root_message = invalid_message(ops.merge_json_files_batch(
    &ctx,
    vec![JsonMergeFilePatch { path: "/".to_string(), patch: json!({"bad": true}), depth: MergeDepth::Unbounded }],
  ));
  assert!(root_message.contains("Cannot store at root path"));

  let duplicate_message = invalid_message(ops.merge_json_files_batch(
    &ctx,
    vec![
      JsonMergeFilePatch { path: "/state/dup.json".to_string(), patch: json!({"one": true}), depth: MergeDepth::Unbounded },
      JsonMergeFilePatch { path: "state/dup.json".to_string(), patch: json!({"two": true}), depth: MergeDepth::Unbounded },
    ],
  ));
  assert!(duplicate_message.contains("Duplicate batch path"));
  assert!(ops.read_file_buffered("/state/dup.json").is_err());
}

#[test]
fn concurrent_json_merges_preserve_every_committed_patch() {
  let dir = tempfile::tempdir().unwrap();
  let engine = std::sync::Arc::new(create_engine(&dir));
  let ctx = RequestContext::system();
  DirectoryOps::new(&engine).store_file_buffered(&ctx, "/state/concurrent.json", br#"{"base":true}"#, Some("application/json")).unwrap();

  let thread_count = 24usize;
  let start = std::sync::Arc::new(std::sync::Barrier::new(thread_count));
  let mut workers = Vec::with_capacity(thread_count);
  for index in 0..thread_count {
    let engine = std::sync::Arc::clone(&engine);
    let start = std::sync::Arc::clone(&start);
    workers.push(std::thread::spawn(move || {
      start.wait();
      DirectoryOps::new(&engine)
        .merge_json_file(
          &RequestContext::system(),
          "/state/concurrent.json",
          json!({format!("field_{index:02}"): index}),
          MergeDepth::Unbounded,
        )
        .unwrap();
    }));
  }
  for worker in workers {
    worker.join().unwrap();
  }

  let merged = read_json(&DirectoryOps::new(&engine), "/state/concurrent.json");
  assert_eq!(merged["base"], true);
  for index in 0..thread_count {
    assert_eq!(merged[format!("field_{index:02}")], index, "committed concurrent patch {index} was lost");
  }
}
