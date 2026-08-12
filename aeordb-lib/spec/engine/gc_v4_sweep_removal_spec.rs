use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::v4::first_authority::{SweepLocatorRemovalRequestV1, V4FirstAuthorityPublisher};
use aeordb::engine::v4::gc_sweep_removal::{
  SweepLocatorRemovalAuthorityV1, SweepLocatorRemovalBatchOutcomeV1, SweepLocatorRemovalCompletionPermitV1, SweepLocatorRemovalErrorV1,
};

#[allow(dead_code)]
fn assert_single_guarded_boundary_contract(
  publisher: &V4FirstAuthorityPublisher,
  request: SweepLocatorRemovalRequestV1<'_>,
  authority: &mut dyn SweepLocatorRemovalAuthorityV1,
) -> Result<SweepLocatorRemovalCompletionPermitV1, SweepLocatorRemovalErrorV1> {
  publisher.execute_sweep_locator_removals(request, authority)
}

fn rust_sources(root: &Path, sources: &mut Vec<PathBuf>) {
  for entry in fs::read_dir(root).unwrap() {
    let entry = entry.unwrap();
    if entry.file_type().unwrap().is_dir() {
      rust_sources(&entry.path(), sources);
    } else if entry.path().extension().and_then(|extension| extension.to_str()) == Some("rs") {
      sources.push(entry.path());
    }
  }
}

#[test]
fn locator_removal_has_one_disconnected_guarded_owner_and_no_receipt_or_void_authority() {
  let batch = SweepLocatorRemovalBatchOutcomeV1 { reclaim_commit_sequence: 42, outcomes: Vec::new() };
  assert_eq!(batch.reclaim_commit_sequence, 42);

  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let mut sources = Vec::new();
  rust_sources(&source_root, &mut sources);
  let mut owners = sources
    .iter()
    .filter(|path| fs::read_to_string(path).unwrap().contains("pub fn execute_sweep_locator_removals("))
    .cloned()
    .collect::<Vec<_>>();
  owners.sort();
  assert_eq!(owners, vec![source_root.join("engine/v4/first_authority.rs")]);

  let authority_source = fs::read_to_string(&owners[0]).unwrap();
  let method_start = authority_source.find("pub fn execute_sweep_locator_removals(").unwrap();
  let method_end = authority_source[method_start..]
    .find("/// Hard-publish one already-qualified sweep proposal")
    .map(|offset| method_start + offset)
    .unwrap_or(authority_source.len());
  let method = &authority_source[method_start..method_end];
  assert!(method.contains("with_global_exclusion"));
  assert!(method.contains("root_state.lock"));
  assert!(method.contains("remove_sweep_locators"));
  assert!(method.contains("complete_sweep_locator_removal"));
  assert!(!method.contains("CommittedExclusion"));
  assert!(!method.contains("CompoundExclusion"));
  for forbidden in
    ["encode_sweep_receipt_v1", "VoidManager", "VoidCatalog", "replace_all", "run_gc", "server::", "DirectoryOps", "StorageEngine"]
  {
    assert!(!method.contains(forbidden), "guarded removal unexpectedly references {forbidden}");
  }
}
