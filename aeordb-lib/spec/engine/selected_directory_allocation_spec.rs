//! Independent malformed framing measures refusal before excessive decoding.
use super::*;
use crate::engine::v4::read_view_native::decode_validated_selected_directory_node;

fn directory_entity(bytes: Vec<u8>, width: usize) -> LoadedImmutableEntityV1 {
  LoadedImmutableEntityV1 {
    entity_version: 0,
    entry_type: EntryTypeV4::DirectoryIndex,
    flags: 0,
    compression_algorithm: CompressionAlgorithm::None,
    timestamp_ms: 1,
    write_sequence: 1,
    key: vec![1; width],
    stored_value: bytes,
  }
}

fn independent_children(count: usize, width: usize) -> Vec<u8> {
  let mut bytes = Vec::new();
  for index in 0..count {
    let mut name = format!("{index:04}");
    name.extend(std::iter::repeat_n('x', 997 - name.len()));
    bytes.push(EntryTypeV4::FileRecord.to_u8());
    bytes.extend_from_slice(&vec![1; width]);
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&1i64.to_le_bytes());
    bytes.extend_from_slice(&1i64.to_le_bytes());
    bytes.extend_from_slice(&997u16.to_le_bytes());
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
  }
  bytes
}

#[test]
fn selected_directory_decode_rejects_declared_internal_fanout_before_allocating_it() {
  for width in [32, 64] {
    let entity = directory_entity(vec![1, 255, 255], width);
    let (result, allocations) =
      allocation_probe::measure(0, || decode_validated_selected_directory_node(&entity, width, "/", None, None, false));
    assert!(result.is_err());
    assert!(allocations.maximum < 64 << 10, "three-byte header allocated its unchecked count: {allocations:?}");
  }
}

#[test]
fn selected_directory_decode_stops_flat_children_at_its_existing_bound() {
  for width in [32, 64] {
    let entity = directory_entity(independent_children(257, width), width);
    let (result, allocations) =
      allocation_probe::measure_nth(997, usize::MAX, || decode_validated_selected_directory_node(&entity, width, "/", None, None, false));
    assert!(result.is_err());
    assert!(!allocations.injected_failure);
    assert!(allocations.matching_requests <= 256, "decoded beyond flat fanout: {allocations:?}");
  }
}

#[test]
fn selected_directory_decode_stops_leaf_payload_at_its_existing_bound() {
  for width in [32, 64] {
    let mut bytes = vec![0, 40, 0];
    bytes.extend_from_slice(&independent_children(41, width));
    let entity = directory_entity(bytes, width);
    let (result, allocations) =
      allocation_probe::measure_nth(997, usize::MAX, || decode_validated_selected_directory_node(&entity, width, "/", None, None, true));
    assert!(result.is_err());
    assert!(!allocations.injected_failure);
    assert!(allocations.matching_requests <= 40, "decoded beyond B-tree leaf fanout: {allocations:?}");
  }
}

// Append only after preserving the original three RED test bodies.
#[test]
fn selected_directory_decode_preserves_valid_flat_leaf_and_internal_boundaries() {
  use crate::engine::btree::BTreeNode;
  for width in [32, 64] {
    for count in [0, 1, 256] {
      let entity = directory_entity(independent_children(count, width), width);
      let node = decode_validated_selected_directory_node(&entity, width, "/", None, None, false).unwrap();
      let BTreeNode::Leaf(leaf) = node else {
        panic!("flat directory did not remain a leaf")
      };
      assert_eq!(leaf.entries.len(), count);
    }
    for count in [0, 1, 40] {
      let mut bytes = vec![0];
      bytes.extend_from_slice(&(count as u16).to_le_bytes());
      bytes.extend_from_slice(&independent_children(count, width));
      let entity = directory_entity(bytes, width);
      let node = decode_validated_selected_directory_node(&entity, width, "/", None, None, true).unwrap();
      let BTreeNode::Leaf(leaf) = node else {
        panic!("B-tree leaf changed role")
      };
      assert_eq!(leaf.entries.len(), count);
    }
    for count in [1u16, 77] {
      let mut bytes = vec![1];
      bytes.extend_from_slice(&count.to_le_bytes());
      for index in 0..count {
        let key = format!("{index:04}");
        bytes.extend_from_slice(&(key.len() as u16).to_le_bytes());
        bytes.extend_from_slice(key.as_bytes());
      }
      for index in 0..=count {
        bytes.extend_from_slice(&vec![(index + 1) as u8; width]);
      }
      let entity = directory_entity(bytes, width);
      let node = decode_validated_selected_directory_node(&entity, width, "/", None, None, true).unwrap();
      let BTreeNode::Internal(internal) = node else {
        panic!("internal B-tree changed role")
      };
      assert_eq!(internal.keys.len(), count as usize);
      assert_eq!(internal.children.len(), count as usize + 1);
    }
  }
}

#[test]
fn selected_directory_decode_preserves_versions_truncation_and_canonical_count_checks() {
  for width in [32, 64] {
    let mut bytes = vec![0, 1, 0];
    bytes.extend_from_slice(&independent_children(1, width));
    for end in 0..bytes.len() {
      let entity = directory_entity(bytes[..end].to_vec(), width);
      assert!(decode_validated_selected_directory_node(&entity, width, "/", None, None, true).is_err(), "prefix{end}");
    }
    for count in [0, 2, 40, 41, 255] {
      let mut changed = bytes.clone();
      changed[1] = count;
      let entity = directory_entity(changed, width);
      assert!(decode_validated_selected_directory_node(&entity, width, "/", None, None, true).is_err());
    }
    for version in [1, 2, 255] {
      let mut entity = directory_entity(bytes.clone(), width);
      entity.entity_version = version;
      assert!(decode_validated_selected_directory_node(&entity, width, "/", None, None, true).is_err());
    }
    bytes.push(0);
    let entity = directory_entity(bytes, width);
    assert!(decode_validated_selected_directory_node(&entity, width, "/", None, None, true).is_err());
  }
}

#[test]
fn selected_directory_decode_does_not_reinterpret_the_generic_legacy_decoder() {
  use crate::engine::btree::BTreeNode;
  use crate::engine::directory_entry::deserialize_child_entries;
  for width in [32, 64] {
    let flat = independent_children(257, width);
    assert_eq!(deserialize_child_entries(&flat, width, 0).unwrap().len(), 257);
    let mut bytes = vec![0, 40, 0];
    bytes.extend_from_slice(&independent_children(41, width));
    let BTreeNode::Leaf(leaf) = BTreeNode::deserialize(&bytes, width, 0).unwrap() else {
      panic!("legacy leaf changed role")
    };
    assert_eq!(leaf.entries.len(), 41);
    let entity = directory_entity(bytes, width);
    assert!(decode_validated_selected_directory_node(&entity, width, "/", None, None, true).is_err());
  }
}

#[test]
fn selected_directory_decode_collection_allocation_failure_is_operational_and_retryable() {
  use crate::engine::v4::read_view_native::NativeSelectedNamespaceReadErrorClassV1;
  for width in [32, 64] {
    for leaf in [false, true] {
      let count = if leaf { 40 } else { 256 };
      let mut bytes = if leaf { vec![0, 40, 0] } else { Vec::new() };
      bytes.extend_from_slice(&independent_children(count, width));
      let entity = directory_entity(bytes, width);
      let table_bytes = count * std::mem::size_of::<ChildEntry>();
      let (result, allocations) =
        allocation_probe::measure(table_bytes, || decode_validated_selected_directory_node(&entity, width, "/", None, None, leaf));
      assert!(allocations.injected_failure, "collection refusal not exercised: {allocations:?}");
      let error = result.expect_err("allocation refusal cannot return decoded children");
      assert!(matches!(
        error.class(),
        NativeSelectedNamespaceReadErrorClassV1::ResourceLimit | NativeSelectedNamespaceReadErrorClassV1::Unavailable
      ));
      decode_validated_selected_directory_node(&entity, width, "/", None, None, leaf).unwrap();
    }
  }
}
