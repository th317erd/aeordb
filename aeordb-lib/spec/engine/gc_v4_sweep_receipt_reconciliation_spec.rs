use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::v4::first_authority::{
  SweepReceiptHardPublicationReceiptV1, SweepReceiptReconciliationRequestV1, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::gc_sweep_reconciliation::{SweepReceiptReconciliationErrorV1, SweepReceiptVoidAuthorityV1};

fn reconcile_receipt(
  publisher: &V4FirstAuthorityPublisher,
  request: SweepReceiptReconciliationRequestV1<'_>,
  authority: &mut dyn SweepReceiptVoidAuthorityV1,
) -> Result<SweepReceiptHardPublicationReceiptV1, SweepReceiptReconciliationErrorV1> {
  publisher.reconcile_sweep_receipt(request, authority)
}

fn collect_rust_files(root: &Path, output: &mut Vec<PathBuf>) {
  for entry in fs::read_dir(root).unwrap() {
    let path = entry.unwrap().path();
    if path.is_dir() {
      collect_rust_files(&path, output);
    } else if path.extension().is_some_and(|extension| extension == "rs") {
      output.push(path);
    }
  }
}

#[test]
fn sweep_receipt_reconciliation_has_one_disconnected_first_authority_owner() {
  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let mut source_files = Vec::new();
  collect_rust_files(&source_root, &mut source_files);

  let owners: Vec<_> =
    source_files.iter().filter(|path| fs::read_to_string(path).unwrap().contains("pub fn reconcile_sweep_receipt(")).collect();
  assert_eq!(owners.len(), 1);
  assert!(owners[0].ends_with("engine/v4/first_authority.rs"));

  let source = fs::read_to_string(owners[0]).unwrap();
  let method_start = source.find("pub fn reconcile_sweep_receipt(").unwrap();
  let method_tail = &source[method_start..];
  let method_end = method_tail.find("\n  ///").unwrap_or(method_tail.len());
  let method = &method_tail[..method_end];
  assert!(method.contains("root_state.lock()"));
  assert!(method.contains("recheck_sweep_receipt_void_authority"));
  assert!(method.contains("publish_immutable_gc_artifact_locked"));

  for forbidden in ["VoidManager", "replace_all", "run_gc", "server::", "DirectoryOps", "StorageEngine"] {
    assert!(!method.contains(forbidden), "receipt reconciler must not call live {forbidden}");
  }

  let function_pointer: fn(
    &V4FirstAuthorityPublisher,
    SweepReceiptReconciliationRequestV1<'_>,
    &mut dyn SweepReceiptVoidAuthorityV1,
  ) -> Result<SweepReceiptHardPublicationReceiptV1, SweepReceiptReconciliationErrorV1> = reconcile_receipt;
  assert_eq!(std::mem::size_of_val(&function_pointer), std::mem::size_of::<usize>());
}
