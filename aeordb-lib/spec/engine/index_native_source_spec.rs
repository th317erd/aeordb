use aeordb::engine::batch_commit::BufferedFile;
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::memory_coordinator::MemoryOwner;
use aeordb::engine::v4::index_native_source::{NativeIndexFileRevisionSourceV1, NativeIndexSourceLimitsV1};
use aeordb::engine::v4::index_producer_source::{IndexFileRevisionReadErrorClassV1, IndexFileRevisionSourceV1};
use aeordb::engine::{RequestContext, StorageEngine};

fn create_engine(directory: &tempfile::TempDir) -> StorageEngine {
  let path = directory.path().join("native-index-source.aeordb");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  DirectoryOps::new(&engine).ensure_root_directory(&RequestContext::system()).unwrap();
  engine
}

fn limits() -> NativeIndexSourceLimitsV1 {
  NativeIndexSourceLimitsV1::new(16 * 1_024 * 1_024, 16 * 1_024 * 1_024, 64).unwrap()
}

#[test]
fn native_revision_source_reads_the_exact_historical_root() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();

  operations.store_file_buffered(&context, "/docs/note.txt", b"first", Some("text/plain")).unwrap();
  let first_root = engine.head_hash().unwrap();
  operations.store_file_buffered(&context, "/docs/note.txt", b"second version", Some("text/plain")).unwrap();
  let second_root = engine.head_hash().unwrap();

  let source = NativeIndexFileRevisionSourceV1::new(&engine, limits());
  let first_read = source.load_file_revision(&first_root, "/docs/note.txt").unwrap().unwrap();
  let second_read = source.load_file_revision(&second_root, "/docs/note.txt").unwrap().unwrap();
  let first = first_read.revision();
  let second = second_read.revision();

  assert_ne!(first.revision_hash, second.revision_hash);
  assert_eq!(first.file_record.total_size, 5);
  assert_eq!(second.file_record.total_size, 14);
  assert_eq!(first.file_record.path, "/docs/note.txt");
}

#[test]
fn native_revision_source_distinguishes_absent_paths_and_non_files() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  operations.create_directory(&context, "/docs/empty").unwrap();
  let root = engine.head_hash().unwrap();
  let source = NativeIndexFileRevisionSourceV1::new(&engine, limits());

  assert!(source.load_file_revision(&root, "/docs/missing.txt").unwrap().is_none());
  assert!(source.load_file_revision(&root, "/docs/empty").unwrap().is_none());
}

#[test]
fn native_revision_source_rejects_malformed_requests_before_storage_access() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let source = NativeIndexFileRevisionSourceV1::new(&engine, limits());
  let root = engine.head_hash().unwrap();

  for (candidate_root, path) in [
    (Vec::new(), "/docs/file.txt"),
    (vec![0; engine.hash_algo().hash_length()], "/docs/file.txt"),
    (root.clone(), "docs/file.txt"),
    (root.clone(), "/docs/../file.txt"),
    (root.clone(), "/"),
  ] {
    let error = source.load_file_revision(&candidate_root, path).unwrap_err();
    assert_eq!(error.class(), IndexFileRevisionReadErrorClassV1::Corrupt, "root={candidate_root:?} path={path}");
    assert_eq!(error.code(), "native_revision_request");
  }
}

#[test]
fn native_revision_source_treats_a_missing_retained_root_as_corruption() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let source = NativeIndexFileRevisionSourceV1::new(&engine, limits());
  let missing_root = vec![0xa5; engine.hash_algo().hash_length()];

  let error = source.load_file_revision(&missing_root, "/docs/file.txt").unwrap_err();
  assert_eq!(error.class(), IndexFileRevisionReadErrorClassV1::Corrupt);
  assert_eq!(error.code(), "native_revision_root_missing");
}

#[test]
fn native_revision_source_classifies_shutdown_as_retryable() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let root = engine.head_hash().unwrap();
  engine.begin_shutdown();
  let source = NativeIndexFileRevisionSourceV1::new(&engine, limits());

  let error = source.load_file_revision(&root, "/docs/file.txt").unwrap_err();
  assert_eq!(error.class(), IndexFileRevisionReadErrorClassV1::Retryable);
  assert_eq!(error.code(), "native_revision_unavailable");
}

#[test]
fn native_revision_source_retains_task_memory_until_the_read_is_dropped() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  operations.store_file_buffered(&context, "/docs/note.txt", b"memory", Some("text/plain")).unwrap();
  let root = engine.head_hash().unwrap();
  let source = NativeIndexFileRevisionSourceV1::new(&engine, limits());
  let before = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes;

  let read = source.load_file_revision(&root, "/docs/note.txt").unwrap().unwrap();
  let during = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes;
  assert!(during > before);
  assert_eq!(during - before, read.reserved_bytes());
  drop(read);
  assert_eq!(engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, before);
}

#[test]
fn native_revision_source_releases_memory_when_a_stable_entity_exceeds_its_bound() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  operations.store_file_buffered(&context, "/docs/note.txt", b"bounded", Some("text/plain")).unwrap();
  let root = engine.head_hash().unwrap();
  let source = NativeIndexFileRevisionSourceV1::new(&engine, NativeIndexSourceLimitsV1::new(1, 16 * 1_024 * 1_024, 64).unwrap());
  let before = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes;

  let error = source.load_file_revision(&root, "/docs/note.txt").unwrap_err();
  assert_eq!(error.class(), IndexFileRevisionReadErrorClassV1::Retryable);
  assert_eq!(error.code(), "native_revision_entity_limit");
  assert_eq!(engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, before);
}

#[test]
fn native_revision_source_seeks_btree_directories_with_a_bounded_depth() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  let files = (0..300)
    .map(|index| BufferedFile {
      path: format!("/wide/{index:04}.txt"),
      data: format!("value-{index}").into_bytes(),
      content_type: Some("text/plain".to_string()),
    })
    .collect();
  operations.store_files_buffered_batch(&context, files).unwrap();
  let root = engine.head_hash().unwrap();

  let source = NativeIndexFileRevisionSourceV1::new(&engine, limits());
  let read = source.load_file_revision(&root, "/wide/0299.txt").unwrap().unwrap();
  assert_eq!(read.revision().file_record.total_size, 9);
  assert!(source.load_file_revision(&root, "/wide/9999.txt").unwrap().is_none());

  let shallow =
    NativeIndexFileRevisionSourceV1::new(&engine, NativeIndexSourceLimitsV1::new(16 * 1_024 * 1_024, 16 * 1_024 * 1_024, 1).unwrap());
  let error = shallow.load_file_revision(&root, "/wide/0299.txt").unwrap_err();
  assert_eq!(error.class(), IndexFileRevisionReadErrorClassV1::Corrupt);
  assert_eq!(error.code(), "native_revision_btree_depth");
}
