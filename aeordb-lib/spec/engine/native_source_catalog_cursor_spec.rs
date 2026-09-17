//! Defensive cursor models; these are not claimed as valid native hash cycles.
use super::*;
use crate::engine::memory_coordinator::MemoryPolicy;

#[test]
fn source_catalog_cursor_model_refuses_ancestor_cycles_and_depth_before_reloading() {
  for cycle in [true, false] {
    let memory = MemoryCoordinator::new(MemoryPolicy::new(1 << 20, 2 << 20, 1, 1 << 16).unwrap());
    let mut cursor = SourceCatalogCursorV1::new(&[1; 32], 3).unwrap();
    let mut reads = 0;
    let error = cursor
      .advance(
        |_| Ok(()),
        |hash, _, _| {
          reads += 1;
          let next = if cycle { hash[0] } else { hash[0] + 1 };
          Ok(LoadedNamespaceSeekNodeV1 {
            node: BTreeNode::Internal(InternalNode { keys: vec!["/z".to_string()], children: vec![vec![next; 32], vec![200; 32]] }),
            _memory: memory.reserve(MemoryOwner::Task, 4096, AdmissionClass::Maintenance).unwrap(),
          })
        },
      )
      .unwrap_err();
    assert_eq!(error.code(), if cycle { "semantic_source_catalog_cycle" } else { "semantic_source_catalog_depth" });
    assert_eq!(reads, if cycle { 1 } else { 3 });
    drop(cursor);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn source_catalog_cursor_model_visits_each_node_once_in_order_and_releases_frames() {
  let memory = MemoryCoordinator::new(MemoryPolicy::new(1 << 20, 2 << 20, 1, 1 << 16).unwrap());
  let mut cursor = SourceCatalogCursorV1::new(&[1; 32], 3).unwrap();
  let mut reads = Vec::new();
  let mut paths = Vec::new();
  let mut admitted_rows = 0;
  loop {
    let result = cursor
      .advance(
        |row| {
          admitted_rows += usize::from(row);
          Ok(())
        },
        |hash, lower, upper| {
          reads.push(hash[0]);
          let node = if hash[0] == 1 {
            BTreeNode::Internal(InternalNode { keys: vec!["/b".to_string()], children: vec![vec![2; 32], vec![3; 32]] })
          } else {
            let path = if hash[0] == 2 { "/a" } else { "/b" };
            validate_range(path, lower, upper, false)?;
            BTreeNode::Leaf(LeafNode {
              entries: vec![ChildEntry {
                entry_type: crate::engine::EntryType::FileRecord.to_u8(),
                hash: vec![0; 32],
                total_size: 0,
                created_at: 0,
                updated_at: 0,
                name: path.to_string(),
                content_type: None,
                virtual_time: 0,
                node_id: 0,
              }],
            })
          };
          Ok(LoadedNamespaceSeekNodeV1 { node, _memory: memory.reserve(MemoryOwner::Task, 4096, AdmissionClass::Maintenance).unwrap() })
        },
      )
      .unwrap();
    match result {
      Some(row) => paths.push(row.name),
      None => break,
    }
  }
  assert_eq!(reads, [1, 2, 3]);
  assert_eq!(paths, ["/a", "/b"]);
  assert_eq!(admitted_rows, 2);
  assert_eq!(cursor.nodes, 3);
  drop(cursor);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}
