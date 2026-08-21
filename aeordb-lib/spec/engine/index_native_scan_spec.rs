use std::cell::Cell;

use aeordb::engine::batch_commit::BufferedFile;
use aeordb::engine::btree::{BTreeNode, InternalNode, LeafNode};
use aeordb::engine::directory_entry::ChildEntry;
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::memory_coordinator::MemoryOwner;
use aeordb::engine::v4::index_maintenance_scan::{
  IndexMaintenanceScanLimitsV1, IndexMaintenanceScanReadErrorClassV1, IndexMaintenanceScanRequestV1, IndexMaintenanceScanSourceV1,
};
use aeordb::engine::v4::index_native_source::{NativeIndexMaintenanceScanSourceV1, NativeIndexScanTraversalLimitsV1, NativeIndexSourceLimitsV1};
use aeordb::engine::{EntryType, RequestContext, StorageEngine};

fn create_engine(directory: &tempfile::TempDir) -> StorageEngine {
  let path = directory.path().join("native-index-scan.aeordb");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  DirectoryOps::new(&engine).ensure_root_directory(&RequestContext::system()).unwrap();
  engine
}

fn source(engine: &StorageEngine, maximum_steps: u32) -> NativeIndexMaintenanceScanSourceV1<'_> {
  NativeIndexMaintenanceScanSourceV1::new(
    engine,
    NativeIndexSourceLimitsV1::new(16 * 1_024 * 1_024, 16 * 1_024 * 1_024, 64).unwrap(),
    NativeIndexScanTraversalLimitsV1::new(128, maximum_steps).unwrap(),
  )
}

fn scan_limits(maximum_documents: u32, maximum_retained_bytes: u64) -> IndexMaintenanceScanLimitsV1 {
  IndexMaintenanceScanLimitsV1::new(maximum_documents, maximum_retained_bytes, 4 * 1_024).unwrap()
}

fn request<'a>(
  root: &'a [u8],
  scope: &'a str,
  resume_after: Option<&'a str>,
  limits: IndexMaintenanceScanLimitsV1,
  is_cancelled: &'a dyn Fn() -> bool,
) -> IndexMaintenanceScanRequestV1<'a> {
  IndexMaintenanceScanRequestV1 { namespace_root: root, scope, resume_after, limits, is_cancelled }
}

fn paths(read: &aeordb::engine::v4::index_maintenance_scan::IndexMaintenanceScanReadV1) -> Vec<&str> {
  read.page().documents.iter().map(|document| document.file_record.path.as_str()).collect()
}

fn scan_error(
  result: Result<
    aeordb::engine::v4::index_maintenance_scan::IndexMaintenanceScanReadV1,
    aeordb::engine::v4::index_maintenance_scan::IndexMaintenanceScanReadErrorV1,
  >,
) -> aeordb::engine::v4::index_maintenance_scan::IndexMaintenanceScanReadErrorV1 {
  match result {
    Ok(_) => panic!("native scan unexpectedly succeeded"),
    Err(error) => error,
  }
}

fn child(name: &str, hash: Vec<u8>, entry_type: EntryType) -> ChildEntry {
  ChildEntry {
    entry_type: entry_type.to_u8(),
    hash,
    total_size: 1,
    created_at: 1,
    updated_at: 1,
    name: name.to_string(),
    content_type: None,
    virtual_time: 1,
    node_id: 1,
  }
}

#[test]
fn native_scan_pages_the_exact_historical_root_and_resumes_without_duplicates() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  operations.store_file_buffered(&context, "/docs/a.txt", b"old", Some("text/plain")).unwrap();
  operations.store_file_buffered(&context, "/docs/b.txt", b"bee", Some("text/plain")).unwrap();
  operations.store_file_buffered(&context, "/docs/nested/c.txt", b"sea", Some("text/plain")).unwrap();
  let historical_root = engine.head_hash().unwrap();
  operations.store_file_buffered(&context, "/docs/a.txt", b"new-version", Some("text/plain")).unwrap();
  operations.store_file_buffered(&context, "/docs/d.txt", b"dee", Some("text/plain")).unwrap();

  let source = source(&engine, 512);
  let first = source.scan(request(&historical_root, "/docs", None, scan_limits(2, 128 * 1_024), &|| false)).unwrap();
  assert_eq!(paths(&first), ["/docs/a.txt", "/docs/b.txt"]);
  assert_eq!(first.page().documents[0].file_record.total_size, 3);
  assert!(!first.page().complete);
  assert_eq!(first.page().next_resume_after.as_deref(), Some("/docs/b.txt"));

  let second = source
    .scan(request(&historical_root, "/docs", first.page().next_resume_after.as_deref(), scan_limits(2, 128 * 1_024), &|| false))
    .unwrap();
  assert_eq!(paths(&second), ["/docs/nested/c.txt"]);
  assert!(second.page().complete);
  assert_eq!(second.page().next_resume_after, None);
}

#[test]
fn native_scan_orders_full_paths_when_directory_names_prefix_siblings() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  for path in ["/docs/a/inside.txt", "/docs/a.txt", "/docs/a-/dash.txt", "/docs/a!/bang.txt", "/docs/z.txt"] {
    operations.store_file_buffered(&context, path, path.as_bytes(), Some("text/plain")).unwrap();
  }
  let root = engine.head_hash().unwrap();

  let read = source(&engine, 512).scan(request(&root, "/docs", None, scan_limits(16, 256 * 1_024), &|| false)).unwrap();
  assert_eq!(paths(&read), ["/docs/a!/bang.txt", "/docs/a-/dash.txt", "/docs/a.txt", "/docs/a/inside.txt", "/docs/z.txt"]);
  assert!(read.page().complete);
}

#[test]
fn native_scan_seeks_into_a_late_btree_page_under_a_small_work_budget() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  let files = (0..600)
    .map(|index| BufferedFile {
      path: format!("/wide/{index:04}.json"),
      data: format!("{{\"index\":{index}}}").into_bytes(),
      content_type: Some("application/json".to_string()),
    })
    .collect();
  operations.store_files_buffered_batch(&context, files).unwrap();
  let root = engine.head_hash().unwrap();

  let read = source(&engine, 128).scan(request(&root, "/wide", Some("/wide/0590.json"), scan_limits(3, 128 * 1_024), &|| false)).unwrap();
  assert_eq!(paths(&read), ["/wide/0591.json", "/wide/0592.json", "/wide/0593.json"]);
  assert!(!read.page().complete);
}

#[test]
fn native_scan_paginates_a_mixed_btree_tree_in_exact_full_path_order() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  let mut expected = Vec::new();
  let mut files = Vec::new();
  for index in 0..300 {
    let path = format!("/catalog/{index:04}.txt");
    expected.push(path.clone());
    files.push(BufferedFile { path, data: vec![index as u8], content_type: Some("text/plain".to_string()) });
  }
  for path in ["/catalog/0100!/nested.txt", "/catalog/0100-/nested.txt", "/catalog/0100/deeper.txt", "/catalog/zz/final.txt"] {
    expected.push(path.to_string());
    files.push(BufferedFile { path: path.to_string(), data: path.as_bytes().to_vec(), content_type: Some("text/plain".to_string()) });
  }
  operations.store_files_buffered_batch(&context, files).unwrap();
  expected.sort();
  let root = engine.head_hash().unwrap();
  let source = source(&engine, 4_096);
  let mut actual = Vec::new();
  let mut resume_after = None;

  loop {
    let read = source.scan(request(&root, "/catalog", resume_after.as_deref(), scan_limits(17, 512 * 1_024), &|| false)).unwrap();
    actual.extend(read.page().documents.iter().map(|document| document.file_record.path.clone()));
    if read.page().complete {
      break;
    }
    let next = read.page().next_resume_after.clone().unwrap();
    assert_ne!(resume_after.as_deref(), Some(next.as_str()));
    resume_after = Some(next);
  }

  assert_eq!(actual, expected);
}

#[test]
fn native_scan_honors_exact_file_and_absent_scopes() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  operations.store_file_buffered(&context, "/docs/one.txt", b"one", Some("text/plain")).unwrap();
  let root = engine.head_hash().unwrap();
  let source = source(&engine, 128);

  let file = source.scan(request(&root, "/docs/one.txt", None, scan_limits(2, 64 * 1_024), &|| false)).unwrap();
  assert_eq!(paths(&file), ["/docs/one.txt"]);
  assert!(file.page().complete);

  let resumed = source.scan(request(&root, "/docs/one.txt", Some("/docs/one.txt"), scan_limits(2, 64 * 1_024), &|| false)).unwrap();
  assert!(resumed.page().documents.is_empty());
  assert!(resumed.page().complete);

  let absent = source.scan(request(&root, "/missing", None, scan_limits(2, 64 * 1_024), &|| false)).unwrap();
  assert!(absent.page().documents.is_empty());
  assert!(absent.page().complete);
}

#[test]
fn native_scan_retains_exact_page_memory_until_drop_and_refuses_an_oversized_first_document() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  let large_content_type = format!("application/x-aeordb-test-{}", "x".repeat(4 * 1_024));
  operations.store_file_buffered(&context, "/docs/large.json", &[0x5a; 8 * 1_024], Some(&large_content_type)).unwrap();
  let root = engine.head_hash().unwrap();
  let source = source(&engine, 128);
  let before = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes;

  let read = source.scan(request(&root, "/docs", None, scan_limits(1, 128 * 1_024), &|| false)).unwrap();
  let during = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes;
  assert!(during > before);
  assert_eq!(during - before, read.page().retained_bytes);
  drop(read);
  assert_eq!(engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, before);

  let error = scan_error(source.scan(request(&root, "/docs", None, scan_limits(1, 1_024), &|| false)));
  assert_eq!(error.class(), IndexMaintenanceScanReadErrorClassV1::Retryable);
  assert_eq!(error.code(), "native_scan_document_limit");
  assert_eq!(engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, before);

  let error = scan_error(source.scan(request(&root, "/docs", None, scan_limits(u32::MAX, 1_024), &|| false)));
  assert_eq!(error.class(), IndexMaintenanceScanReadErrorClassV1::Retryable);
  assert_eq!(error.code(), "native_scan_page_limit");
  assert_eq!(engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, before);
}

#[test]
fn native_scan_cancellation_missing_roots_and_work_limits_fail_with_stable_classes() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  operations.store_file_buffered(&context, "/docs/a.txt", b"a", Some("text/plain")).unwrap();
  let root = engine.head_hash().unwrap();

  let checks = Cell::new(0u32);
  let cancel = || {
    checks.set(checks.get() + 1);
    checks.get() > 2
  };
  let error = scan_error(source(&engine, 128).scan(request(&root, "/docs", None, scan_limits(4, 64 * 1_024), &cancel)));
  assert_eq!(error.class(), IndexMaintenanceScanReadErrorClassV1::Cancelled);
  assert_eq!(error.code(), "native_scan_cancelled");

  let missing = vec![0xa5; engine.hash_algo().hash_length()];
  let error = scan_error(source(&engine, 128).scan(request(&missing, "/docs", None, scan_limits(4, 64 * 1_024), &|| false)));
  assert_eq!(error.class(), IndexMaintenanceScanReadErrorClassV1::Corrupt);
  assert_eq!(error.code(), "native_scan_root_missing");

  let error = scan_error(source(&engine, 1).scan(request(&root, "/docs", None, scan_limits(4, 64 * 1_024), &|| false)));
  assert_eq!(error.class(), IndexMaintenanceScanReadErrorClassV1::Retryable);
  assert_eq!(error.code(), "native_scan_work_limit");
}

#[test]
fn native_scan_bounds_namespace_depth_and_classifies_shutdown_as_retryable() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  operations.store_file_buffered(&context, "/a/b/c/deep.txt", b"deep", Some("text/plain")).unwrap();
  let root = engine.head_hash().unwrap();
  let shallow = NativeIndexMaintenanceScanSourceV1::new(
    &engine,
    NativeIndexSourceLimitsV1::new(16 * 1_024 * 1_024, 16 * 1_024 * 1_024, 64).unwrap(),
    NativeIndexScanTraversalLimitsV1::new(2, 512).unwrap(),
  );
  let error = scan_error(shallow.scan(request(&root, "/", None, scan_limits(4, 64 * 1_024), &|| false)));
  assert_eq!(error.class(), IndexMaintenanceScanReadErrorClassV1::Retryable);
  assert_eq!(error.code(), "native_scan_path_depth");
  let error = scan_error(shallow.scan(request(&root, "/a/b", None, scan_limits(4, 64 * 1_024), &|| false)));
  assert_eq!(error.class(), IndexMaintenanceScanReadErrorClassV1::Retryable);
  assert_eq!(error.code(), "native_scan_path_depth");

  engine.begin_shutdown();
  let error = scan_error(source(&engine, 128).scan(request(&root, "/", None, scan_limits(4, 64 * 1_024), &|| false)));
  assert_eq!(error.class(), IndexMaintenanceScanReadErrorClassV1::Retryable);
  assert_eq!(error.code(), "native_scan_unavailable");
}

#[test]
fn native_scan_rejects_btree_children_outside_inherited_separator_ranges() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let algorithm = engine.hash_algo();
  let hash_width = algorithm.hash_length();
  let left = BTreeNode::Leaf(LeafNode { entries: vec![child("z", vec![0x31; hash_width], EntryType::FileRecord)] });
  let left_value = left.serialize(hash_width).unwrap();
  let left_hash = left.content_hash(hash_width, &algorithm).unwrap();
  engine.store_entry(EntryType::DirectoryIndex, &left_hash, &left_value).unwrap();
  let right = BTreeNode::Leaf(LeafNode { entries: vec![child("m", vec![0x32; hash_width], EntryType::FileRecord)] });
  let right_value = right.serialize(hash_width).unwrap();
  let right_hash = right.content_hash(hash_width, &algorithm).unwrap();
  engine.store_entry(EntryType::DirectoryIndex, &right_hash, &right_value).unwrap();
  let root = BTreeNode::Internal(InternalNode { keys: vec!["m".to_string()], children: vec![left_hash, right_hash] });
  let root_value = root.serialize(hash_width).unwrap();
  let root_hash = root.content_hash(hash_width, &algorithm).unwrap();
  engine.store_entry(EntryType::DirectoryIndex, &root_hash, &root_value).unwrap();

  let error = scan_error(source(&engine, 128).scan(request(&root_hash, "/", None, scan_limits(4, 64 * 1_024), &|| false)));
  assert_eq!(error.class(), IndexMaintenanceScanReadErrorClassV1::Corrupt);
  assert_eq!(error.code(), "native_revision_btree_range");
}
