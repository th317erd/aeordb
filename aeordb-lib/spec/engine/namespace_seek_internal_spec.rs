use super::*;
use crate::engine::btree::LeafNode;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryPolicy};

#[derive(Debug)]
enum TestError {
  Seek(NamespaceSeekFailureV1),
  Source(&'static str),
}

impl From<NamespaceSeekFailureV1> for TestError {
  fn from(error: NamespaceSeekFailureV1) -> Self {
    Self::Seek(error)
  }
}

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(8 << 20, 16 << 20, 1, 1 << 20).unwrap())
}

fn entry(name: &str, directory: bool) -> ChildEntry {
  ChildEntry {
    entry_type: if directory { EntryType::DirectoryIndex } else { EntryType::FileRecord }.to_u8(),
    hash: vec![9; 32],
    total_size: 0,
    created_at: 0,
    updated_at: 0,
    name: name.to_string(),
    content_type: None,
    virtual_time: 0,
    node_id: 0,
  }
}

fn node(memory: &MemoryCoordinator, node: BTreeNode) -> LoadedNamespaceSeekNodeV1 {
  LoadedNamespaceSeekNodeV1 { node, _memory: memory.reserve(MemoryOwner::Query, 4096, AdmissionClass::Workload).unwrap() }
}

#[test]
fn namespace_seek_successor_preserves_ranges_and_releases_each_decoded_node() {
  let memory = memory();
  let _workspace = memory.reserve(MemoryOwner::Query, 1 << 20, AdmissionClass::Workload).unwrap();
  let mut reads = Vec::new();
  let found = seek_namespace_child_v1::<TestError>(&[1; 32], "f", false, 8, |hash, lower, upper, child| {
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().active_reservations, 1);
    reads.push((hash[0], lower.map(str::to_string), upper.map(str::to_string), child));
    let decoded = match hash[0] {
      1 => BTreeNode::Internal(InternalNode { keys: vec!["m".into()], children: vec![vec![2; 32], vec![5; 32]] }),
      2 => BTreeNode::Internal(InternalNode { keys: vec!["g".into()], children: vec![vec![3; 32], vec![4; 32]] }),
      3 => BTreeNode::Leaf(LeafNode { entries: vec![] }),
      4 => BTreeNode::Leaf(LeafNode { entries: vec![entry("h", false)] }),
      _ => panic!("seek read an unrelated right-hand subtree"),
    };
    Ok(node(&memory, decoded))
  })
  .unwrap()
  .unwrap();
  assert_eq!(found.name, "h");
  assert_eq!(
    reads,
    [
      (1, None, None, false),
      (2, None, Some("m".into()), true),
      (3, None, Some("g".into()), true),
      (2, None, Some("m".into()), true),
      (4, Some("g".into()), Some("m".into()), true),
    ]
  );
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 1 << 20);
}

#[test]
fn namespace_seek_propagates_source_failures_instead_of_claiming_absence_and_retries() {
  for failure_at in 1..=3 {
    let memory = memory();
    let mut calls = 0;
    let failed = seek_namespace_child_v1::<TestError>(&[1; 32], "a", true, 8, |hash, _, _, _| {
      calls += 1;
      if calls == failure_at {
        return Err(TestError::Source("cancelled or unavailable"));
      }
      let decoded = if hash[0] == 1 {
        BTreeNode::Internal(InternalNode { keys: vec!["m".into()], children: vec![vec![2; 32], vec![3; 32]] })
      } else {
        BTreeNode::Leaf(LeafNode { entries: vec![] })
      };
      Ok(node(&memory, decoded))
    });
    assert!(matches!(failed, Err(TestError::Source("cancelled or unavailable"))));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    let retried = seek_namespace_child_v1::<TestError>(&[1; 32], "a", true, 8, |_, _, _, _| {
      Ok(node(&memory, BTreeNode::Leaf(LeafNode { entries: vec![entry("a", false)] })))
    })
    .unwrap()
    .unwrap();
    assert_eq!(retried.name, "a");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn namespace_seek_refuses_cycles_depth_and_impossible_stack_allocation() {
  let memory = memory();
  let cycle = seek_namespace_child_v1::<TestError>(&[1; 32], "", true, 8, |_, _, _, _| {
    Ok(node(&memory, BTreeNode::Internal(InternalNode { keys: vec!["m".into()], children: vec![vec![1; 32], vec![2; 32]] })))
  });
  assert!(matches!(cycle, Err(TestError::Seek(NamespaceSeekFailureV1::Cycle))));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  let depth = seek_namespace_child_v1::<TestError>(&[1; 32], "", true, 0, |_, _, _, _| panic!("zero depth read storage"));
  assert!(matches!(depth, Err(TestError::Seek(NamespaceSeekFailureV1::Depth))));
  let allocation =
    seek_namespace_child_v1::<TestError>(&[1; 32], "", true, usize::MAX, |_, _, _, _| panic!("impossible stack read storage"));
  assert!(matches!(allocation, Err(TestError::Seek(NamespaceSeekFailureV1::Allocation(_)))));
}

#[test]
fn namespace_seek_path_order_matches_an_independent_sort_for_unicode_and_prefixes() {
  let mut entries = [entry("a", true), entry("a-", false), entry("a.", false), entry("aa", true), entry("é", true), entry("é!", false)];
  entries.sort_by(|left, right| left.name.cmp(&right.name));
  let mut expected: Vec<_> = entries
    .iter()
    .map(|entry| {
      let mut name = entry.name.clone();
      if entry.entry_type == EntryType::DirectoryIndex.to_u8() {
        name.push('/');
      }
      name
    })
    .collect();
  expected.sort();
  let mut actual = Vec::new();
  let mut lower = None;
  for _ in 0..=entries.len() {
    let found = next_namespace_child_by_path_v1::<TestError>(lower.as_deref(), |name, inclusive| {
      Ok(entries.iter().find(|entry| entry.name.as_str() > name || (inclusive && entry.name == name)).cloned())
    })
    .unwrap();
    let Some((key, _)) = found else { break };
    actual.push(key.clone());
    lower = Some(key);
  }
  assert_eq!(actual, expected);
  let failed = next_namespace_child_by_path_v1::<TestError>(Some("a-"), |_, _| Err(TestError::Source("read failed")));
  assert!(matches!(failed, Err(TestError::Source("read failed"))));
}

#[test]
fn namespace_seek_workspace_includes_full_separator_strings_and_checked_geometry() {
  let bytes = namespace_seek_workspace_bytes_v1(1, 1, 128, 64).unwrap();
  assert!(bytes > 2 * 128 * u64::from(u16::MAX));
  for values in [(u64::MAX, 1, 1, 32), (1, u64::MAX, 1, 32), (1, 1, u64::MAX, 32), (1, 1, 1, u64::MAX)] {
    assert!(namespace_seek_workspace_bytes_v1(values.0, values.1, values.2, values.3).is_none());
  }
}

#[test]
fn namespace_seek_defends_against_changed_parent_shape_and_invalid_child_index() {
  let memory = memory();
  let mut root_reads = 0;
  let changed = seek_namespace_child_v1::<TestError>(&[1; 32], "", true, 8, |hash, _, _, _| {
    if hash[0] == 1 {
      root_reads += 1;
    }
    let decoded = if hash[0] == 1 && root_reads == 1 {
      BTreeNode::Internal(InternalNode { keys: vec!["m".into()], children: vec![vec![2; 32], vec![3; 32]] })
    } else {
      BTreeNode::Leaf(LeafNode { entries: vec![] })
    };
    Ok(node(&memory, decoded))
  });
  assert!(matches!(changed, Err(TestError::Seek(NamespaceSeekFailureV1::ParentShape))));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  let invalid = seek_namespace_child_v1::<TestError>(&[1; 32], "", true, 8, |_, _, _, _| {
    Ok(node(&memory, BTreeNode::Internal(InternalNode { keys: vec!["m".into()], children: vec![] })))
  });
  assert!(matches!(invalid, Err(TestError::Seek(NamespaceSeekFailureV1::ChildIndex))));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn namespace_seek_distinguishes_an_exhausted_successor_from_an_unknown_child_role() {
  let memory = memory();
  let exhausted = seek_namespace_child_v1::<TestError>(&[1; 32], "z", false, 8, |hash, _, _, _| {
    let decoded = if hash[0] == 1 {
      BTreeNode::Internal(InternalNode { keys: vec!["m".into()], children: vec![vec![2; 32], vec![3; 32]] })
    } else {
      BTreeNode::Leaf(LeafNode { entries: vec![entry("z", false)] })
    };
    Ok(node(&memory, decoded))
  })
  .unwrap();
  assert!(exhausted.is_none());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  let invalid = next_namespace_child_by_path_v1::<TestError>(None, |_, _| {
    let mut child = entry("bad", false);
    child.entry_type = u8::MAX;
    Ok(Some(child))
  });
  assert!(matches!(invalid, Err(TestError::Seek(NamespaceSeekFailureV1::InvalidChild(_)))));
}
