use std::fs::File;

use aeordb::engine::{
  apply_merge_patch, directory_content_hash, directory_path_hash, BufferedFile, DirectoryOps, EngineError, EntryType, JsonMergeFilePatch,
  MergeDepth, RequestContext, StorageEngine,
};
use aeordb::engine::file_header::read_active_header;
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
fn store_file_buffered_defers_durable_head_until_outer_transaction_commits() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let initial_disk_head = disk_head_hash(&dir);

  {
    let _outer = TransactionGuard::new(&engine);
    ops.store_file_buffered(&ctx, "/txn/a.json", br#"{"a":1}"#, Some("application/json")).unwrap();

    let in_memory_head = engine.head_hash().unwrap();
    assert_ne!(in_memory_head, initial_disk_head, "the active engine should see the new HEAD before commit");

    engine.try_flush_hot_buffer();
    assert_eq!(disk_head_hash(&dir), initial_disk_head, "timer hot-tail flushing must not durably publish an in-flight transaction HEAD");
  }

  assert_eq!(disk_head_hash(&dir), engine.head_hash().unwrap(), "outer transaction drop should durably publish HEAD");
  assert_eq!(ops.read_file_buffered("/txn/a.json").unwrap(), br#"{"a":1}"#);
}

#[test]
fn store_files_buffered_batch_defers_durable_head_until_outer_transaction_commits() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let initial_disk_head = disk_head_hash(&dir);

  {
    let _outer = TransactionGuard::new(&engine);
    ops
      .store_files_buffered_batch(
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
      )
      .unwrap();

    let in_memory_head = engine.head_hash().unwrap();
    assert_ne!(in_memory_head, initial_disk_head, "the active engine should see the batched HEAD before commit");

    engine.try_flush_hot_buffer();
    assert_eq!(disk_head_hash(&dir), initial_disk_head, "timer hot-tail flushing must not durably publish an in-flight batch HEAD");
  }

  assert_eq!(disk_head_hash(&dir), engine.head_hash().unwrap(), "outer transaction drop should durably publish batched HEAD");
  assert_eq!(ops.read_file_buffered("/txn/batch/a.json").unwrap(), br#"{"a":1}"#);
  assert_eq!(ops.read_file_buffered("/txn/batch/b.json").unwrap(), br#"{"b":2}"#);
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
