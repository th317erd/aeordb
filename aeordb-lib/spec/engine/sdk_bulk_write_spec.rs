use aeordb::engine::{
  apply_merge_patch, BufferedFile, DirectoryOps, EngineError, JsonMergeFilePatch, MergeDepth, RequestContext, StorageEngine,
};
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
