use super::*;
use crate::engine::btree::{InternalNode, LeafNode};

fn child_node(name: &str, kind: EntryType, hash: Vec<u8>) -> BTreeNode {
  let mut child = directory_child(hash, name.to_string());
  child.entry_type = kind.to_u8();
  BTreeNode::Leaf(LeafNode { entries: vec![child] })
}

#[test]
fn selected_seek_validation_checks_each_namespace_role_and_identity() {
  for width in [32, 64] {
    for kind in [EntryType::DirectoryIndex, EntryType::FileRecord, EntryType::Symlink] {
      validate_selected_seek_node(&child_node("m", kind, vec![1; width]), width, Some("m"), Some("z")).unwrap();
    }
    for kind in [EntryType::Chunk, EntryType::DeletionRecord, EntryType::Snapshot, EntryType::Void, EntryType::Fork] {
      let error = validate_selected_seek_node(&child_node("m", kind, vec![1; width]), width, None, None).unwrap_err();
      assert_eq!(error.code(), "selected_namespace_child_role");
    }
    for name in ["", ".", "..", "a/b", "a\0b"] {
      let error = validate_selected_seek_node(&child_node(name, EntryType::FileRecord, vec![1; width]), width, None, None).unwrap_err();
      assert_eq!(error.code(), "selected_namespace_child_identity");
    }
    for hash in [vec![0; width], vec![1; width - 1], vec![1; width + 1]] {
      let error = validate_selected_seek_node(&child_node("m", EntryType::FileRecord, hash), width, None, None).unwrap_err();
      assert_eq!(error.code(), "selected_namespace_child_identity");
    }
    let mut unknown = child_node("m", EntryType::FileRecord, vec![1; width]);
    let BTreeNode::Leaf(leaf) = &mut unknown else { unreachable!() };
    leaf.entries[0].entry_type = u8::MAX;
    assert_eq!(validate_selected_seek_node(&unknown, width, None, None).unwrap_err().code(), "selected_namespace_entry_type");
  }
}

#[test]
fn selected_seek_validation_enforces_inclusive_lower_exclusive_upper_ranges() {
  let node = child_node("m", EntryType::FileRecord, vec![1; 32]);
  validate_selected_seek_node(&node, 32, Some("m"), Some("n")).unwrap();
  for bounds in [(Some("n"), None), (None, Some("m")), (Some("m"), Some("m")), (Some("z"), Some("a"))] {
    assert_eq!(validate_selected_seek_node(&node, 32, bounds.0, bounds.1).unwrap_err().code(), "selected_namespace_btree_range");
  }
  validate_selected_seek_node(&BTreeNode::Leaf(LeafNode { entries: vec![] }), 32, Some("a"), Some("z")).unwrap();
}

#[test]
fn selected_seek_validation_checks_internal_separator_and_child_identity_boundaries() {
  for width in [32, 64] {
    let internal = InternalNode { keys: vec!["m".to_string()], children: vec![vec![1; width], vec![2; width]] };
    validate_selected_seek_node(&BTreeNode::Internal(internal.clone()), width, Some("a"), Some("z")).unwrap();
    for key in ["", ".", "..", "a/b", "a\0b", "0", "z"] {
      let mut invalid = internal.clone();
      invalid.keys[0] = key.to_string();
      assert_eq!(
        validate_selected_seek_node(&BTreeNode::Internal(invalid), width, Some("a"), Some("z")).unwrap_err().code(),
        "selected_namespace_btree_range"
      );
    }
    for children in [
      vec![],
      vec![vec![1; width]],
      vec![vec![1; width], vec![1; width]],
      vec![vec![0; width], vec![2; width]],
      vec![vec![1; width - 1], vec![2; width]],
    ] {
      let mut invalid = internal.clone();
      invalid.children = children;
      assert_eq!(
        validate_selected_seek_node(&BTreeNode::Internal(invalid), width, None, None).unwrap_err().code(),
        "selected_namespace_btree_child"
      );
    }
  }
}
