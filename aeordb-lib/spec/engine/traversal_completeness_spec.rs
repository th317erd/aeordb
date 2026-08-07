use aeordb::engine::RequestContext;
use aeordb::engine::btree::{
  BTREE_CONVERSION_THRESHOLD, BTREE_LEAF_MARKER, BTreeNode, BTreeWalkMode, InternalNode, btree_list_from_node_with_mode,
  btree_list_with_mode,
};
use aeordb::engine::directory_ops::{DirectoryOps, directory_path_hash};
use aeordb::engine::entry_type::EntryType;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::traversal::{TraversalIntegrity, VisitorCompletion};

fn create_engine(directory: &tempfile::TempDir) -> StorageEngine {
  let path = directory.path().join("traversal.aeordb");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  DirectoryOps::new(&engine).ensure_root_directory(&RequestContext::system()).unwrap();
  engine
}

fn store_files(engine: &StorageEngine, directory: &str, count: usize) {
  let operations = DirectoryOps::new(engine);
  let context = RequestContext::system();
  for number in 0..count {
    operations
      .store_file_buffered(
        &context,
        &format!("{directory}/file_{number:05}.json"),
        format!("{{\"number\":{number}}}").as_bytes(),
        Some("application/json"),
      )
      .unwrap();
  }
}

fn directory_value(engine: &StorageEngine, path: &str) -> Vec<u8> {
  let key = directory_path_hash(path, &engine.hash_algo()).unwrap();
  let (_, _, value) = engine.get_entry(&key).unwrap().unwrap();
  if value.len() == engine.hash_algo().hash_length() {
    engine.get_entry(&value).unwrap().unwrap().2
  } else {
    value
  }
}

#[test]
fn valid_and_damaged_btree_branches_have_distinct_integrity() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let count = BTREE_CONVERSION_THRESHOLD + 100;
  store_files(&engine, "/damaged", count);

  let value = directory_value(&engine, "/damaged");
  let hash_length = engine.hash_algo().hash_length();
  let complete = btree_list_from_node_with_mode(&value, &engine, hash_length, false, BTreeWalkMode::BestEffort).unwrap();
  assert_eq!(complete.integrity, TraversalIntegrity::Complete);
  assert_eq!(complete.entries.len(), count);

  let root = BTreeNode::deserialize(&value, hash_length, 0).unwrap();
  let missing_child = match root {
    BTreeNode::Internal(internal) => internal.children[1].clone(),
    BTreeNode::Leaf(_) => panic!("expected an internal B-tree root"),
  };
  engine.mark_entry_deleted(&missing_child).unwrap();

  let partial = btree_list_from_node_with_mode(&value, &engine, hash_length, false, BTreeWalkMode::BestEffort).unwrap();
  assert_eq!(partial.integrity, TraversalIntegrity::DiagnosticallyPartial);
  assert_eq!(partial.warnings.len(), 1);
  assert!(!partial.entries.is_empty());
  assert!(partial.entries.len() < count);

  let checked = DirectoryOps::new(&engine).list_directory_with_traversal("/damaged").unwrap();
  assert_eq!(checked.integrity, TraversalIntegrity::DiagnosticallyPartial);
  assert_eq!(checked.entries.len(), partial.entries.len());
  assert_eq!(checked.issues.len(), partial.warnings.len());
}

#[test]
fn missing_or_malformed_btree_roots_are_corrupt_not_partial_or_empty_complete() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let hash_length = engine.hash_algo().hash_length();
  let missing_root = engine.compute_hash(b"missing B-tree root").unwrap();

  let missing = btree_list_with_mode(&engine, &missing_root, hash_length, false, BTreeWalkMode::BestEffort).unwrap();
  assert_eq!(missing.integrity, TraversalIntegrity::Corrupt);
  assert!(missing.entries.is_empty());
  assert_eq!(missing.warnings.len(), 1);

  let malformed = btree_list_from_node_with_mode(&[BTREE_LEAF_MARKER], &engine, hash_length, false, BTreeWalkMode::BestEffort).unwrap();
  assert_eq!(malformed.integrity, TraversalIntegrity::Corrupt);
  assert!(malformed.entries.is_empty());
  assert_eq!(malformed.warnings.len(), 1);

  let path_key = directory_path_hash("/malformed-btree", &engine.hash_algo()).unwrap();
  engine.store_entry(EntryType::DirectoryIndex, &path_key, &[BTREE_LEAF_MARKER]).unwrap();
  let checked = DirectoryOps::new(&engine).list_directory_with_traversal("/malformed-btree").unwrap();
  assert_eq!(checked.integrity, TraversalIntegrity::Corrupt);
  assert!(checked.entries.is_empty());
  assert_eq!(checked.issues.len(), 1);
}

#[test]
fn malformed_flat_directories_preserve_corruption_evidence() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let path_key = directory_path_hash("/malformed-flat", &engine.hash_algo()).unwrap();
  engine.store_entry(EntryType::DirectoryIndex, &path_key, &[0x7f]).unwrap();

  let checked = DirectoryOps::new(&engine).list_directory_with_traversal("/malformed-flat").unwrap();
  assert_eq!(checked.integrity, TraversalIntegrity::Corrupt);
  assert!(checked.entries.is_empty());
  assert_eq!(checked.issues.len(), 1);
  assert!(checked.issues[0].reason.contains("directory"));
}

#[test]
fn visitor_early_stop_is_not_structural_corruption() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  store_files(&engine, "/window", BTREE_CONVERSION_THRESHOLD + 100);

  let window = DirectoryOps::new(&engine).list_directory_window("/window", 10, 5).unwrap();
  assert_eq!(window.entries.len(), 5);
  assert!(window.has_more);
  assert_eq!(window.integrity, TraversalIntegrity::Complete);
  assert_eq!(window.visitor_completion, VisitorCompletion::StoppedByVisitor);
  assert!(window.warnings.is_empty());
}

#[test]
fn a_valid_root_with_a_cycle_is_diagnostically_partial() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let cycle_hash = engine.compute_hash(b"cycle root").unwrap();
  let node = BTreeNode::Internal(InternalNode { keys: Vec::new(), children: vec![cycle_hash.clone()] });
  let bytes = node.serialize(engine.hash_algo().hash_length()).unwrap();
  engine.store_entry(EntryType::DirectoryIndex, &cycle_hash, &bytes).unwrap();

  let result = btree_list_with_mode(&engine, &cycle_hash, engine.hash_algo().hash_length(), false, BTreeWalkMode::BestEffort).unwrap();
  assert_eq!(result.integrity, TraversalIntegrity::DiagnosticallyPartial);
  assert!(result.entries.is_empty());
  assert!(result.warnings.iter().any(|warning| warning.reason.contains("cycle")));
}
