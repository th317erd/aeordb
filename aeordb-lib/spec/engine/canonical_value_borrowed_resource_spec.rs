use super::measure;
use aeordb::engine::v4::config_value::{CanonicalValueBounds, borrow_canonical_value};

#[test]
fn dense_canonical_borrowed_traversal_allocates_no_retained_tree_or_scratch() {
  // 52,427 minimal null frames fit within the 256 KiB configuration cap.
  // The existing 768-byte/node admission bound would exceed 32 MiB if
  // materialized. Construct the wire bytes independently before measurement.
  let count = 52_427u32;
  let payload_length = 4 + count * 5;
  let mut bytes = Vec::new();
  bytes.push(9);
  bytes.extend_from_slice(&payload_length.to_le_bytes());
  bytes.extend_from_slice(&count.to_le_bytes());
  for _ in 0..count {
    bytes.extend_from_slice(&[1, 0, 0, 0, 0]);
  }
  assert_eq!(bytes.len(), 262144);
  let (visited, allocations) = measure(0, || {
    let value = borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).unwrap();
    let mut visited = 0;
    for item in value.array_entries().unwrap() {
      assert_eq!(item.unwrap().encoded_bytes(), [1, 0, 0, 0, 0]);
      visited += 1;
    }
    visited
  });
  assert_eq!(visited, count);
  assert_eq!(allocations.total, 0, "borrowed traversal unexpectedly allocated");
  assert_eq!(allocations.maximum, 0);
}

#[test]
fn many_single_entry_maps_remain_borrowed_without_allocating_map_nodes_or_keys() {
  let count = 14_563u32;
  let payload_length = 4 + count * 18;
  let mut bytes = vec![9];
  bytes.extend_from_slice(&payload_length.to_le_bytes());
  bytes.extend_from_slice(&count.to_le_bytes());
  for _ in 0..count {
    bytes.extend_from_slice(&[10, 13, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
  }
  assert_eq!(bytes.len(), 262143);
  let (visited, allocations) = measure(0, || {
    let value = borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).unwrap();
    let mut visited = 0;
    for item in value.array_entries().unwrap() {
      let mut members = item.unwrap().map_entries().unwrap();
      let (key, value) = members.next().unwrap().unwrap();
      assert_eq!(key, "");
      assert_eq!(value.encoded_bytes(), [1, 0, 0, 0, 0]);
      assert!(members.next().is_none());
      visited += 1;
    }
    visited
  });
  assert_eq!(visited, count);
  assert_eq!(allocations.total, 0, "borrowed map traversal allocated");
}
