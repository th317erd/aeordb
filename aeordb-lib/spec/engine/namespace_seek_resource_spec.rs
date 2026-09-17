use super::measure;
use aeordb::engine::{RequestContext, StorageEngine};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::v4::index_maintenance_scan::{IndexMaintenanceScanLimitsV1, IndexMaintenanceScanRequestV1, IndexMaintenanceScanSourceV1};
use aeordb::engine::v4::index_native_source::{NativeIndexMaintenanceScanSourceV1, NativeIndexScanTraversalLimitsV1, NativeIndexSourceLimitsV1};

#[test]
fn namespace_seek_native_stack_allocation_refusal_releases_memory_and_retries_without_mutation() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("namespace-seek-allocation.aeordb");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  operations.ensure_root_directory(&context).unwrap();
  operations.store_file_buffered(&context, "/docs/a", b"a", None).unwrap();
  let root = engine.head_hash().unwrap();
  let source = NativeIndexMaintenanceScanSourceV1::new(
    &engine,
    NativeIndexSourceLimitsV1::new(16 << 20, 16 << 20, 64).unwrap(),
    NativeIndexScanTraversalLimitsV1::new(32, 4096).unwrap(),
  );
  let request = || IndexMaintenanceScanRequestV1 {
    namespace_root: &root,
    scope: "/docs",
    resume_after: None,
    limits: IndexMaintenanceScanLimitsV1::new(1, 1 << 20, 4096).unwrap(),
    is_cancelled: &|| false,
  };
  drop(source.scan(request()).unwrap());
  let before = std::fs::read(&path).unwrap();
  let reserved = engine.memory_coordinator().snapshot().unwrap().reserved_bytes;
  // Exact same field geometry as an ancestor frame, without exporting a
  // private implementation type solely to make it test-accessible.
  let stack_bytes = 64 * std::mem::size_of::<(Vec<u8>, usize, Option<String>, Option<String>)>();
  let (result, allocations) = measure(stack_bytes, || source.scan(request()));
  assert!(allocations.injected_failure, "{allocations:?}");
  let error = match result {
    Ok(_) => panic!("stack allocation refusal was hidden"),
    Err(error) => error,
  };
  assert_eq!(error.code(), "native_scan_allocation");
  assert_eq!(engine.memory_coordinator().snapshot().unwrap().reserved_bytes, reserved);
  let retried = source.scan(request()).unwrap();
  assert_eq!(retried.page().documents[0].file_record.path, "/docs/a");
  drop(retried);
  assert_eq!(engine.memory_coordinator().snapshot().unwrap().reserved_bytes, reserved);
  assert_eq!(std::fs::read(&path).unwrap(), before);
}
