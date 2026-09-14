//! Independent framing for the next catalog-update reader prerequisite.
use aeordb::engine::v4::config_value::{CanonicalValueBounds, borrow_canonical_value};

fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
  let mut result = vec![tag];
  result.extend_from_slice(&(payload.len() as u32).to_le_bytes());
  result.extend_from_slice(payload);
  result
}

fn array(values: &[Vec<u8>]) -> Vec<u8> {
  let mut payload = (values.len() as u32).to_le_bytes().to_vec();
  for value in values {
    payload.extend_from_slice(value);
  }
  frame(9, &payload)
}

fn map(values: &[(&str, Vec<u8>)]) -> Vec<u8> {
  let mut payload = (values.len() as u32).to_le_bytes().to_vec();
  for (key, value) in values {
    payload.extend_from_slice(&(key.len() as u32).to_le_bytes());
    payload.extend_from_slice(key.as_bytes());
    payload.extend_from_slice(value);
  }
  frame(10, &payload)
}

#[test]
fn borrowed_maps_arrays_and_bytes_retain_exact_input_slices() {
  for width in [32, 64] {
    let indexes = array(&[frame(8, &vec![3; width]), frame(8, &vec![4; width])]);
    let fields = map(&[("title", map(&[("indexes", indexes), ("value_store_id", frame(8, &vec![2; width]))]))]);
    let bytes = map(&[("fields", fields), ("scope_id", frame(8, &vec![1; width]))]);
    let value = borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).unwrap();
    assert_eq!(value.encoded_bytes().as_ptr(), bytes.as_ptr());
    let mut root = value.map_entries().unwrap();
    let (field_key, fields) = root.next().unwrap().unwrap();
    assert_eq!(field_key, "fields");
    let mut fields = fields.map_entries().unwrap();
    let (field, definition) = fields.next().unwrap().unwrap();
    assert_eq!(field, "title");
    assert!(fields.next().is_none());
    let mut definition = definition.map_entries().unwrap();
    let (key, indexes) = definition.next().unwrap().unwrap();
    assert_eq!(key, "indexes");
    let mut indexes = indexes.array_entries().unwrap();
    for expected in [3, 4] {
      let index = indexes.next().unwrap().unwrap();
      let borrowed = index.as_bytes().unwrap();
      assert_eq!(borrowed, vec![expected; width]);
      assert!(borrowed.as_ptr() >= bytes.as_ptr());
      assert!((borrowed.as_ptr() as usize) + borrowed.len() <= (bytes.as_ptr() as usize) + bytes.len());
    }
    assert!(indexes.next().is_none());
    assert!(indexes.next().is_none());
    let (key, value_store) = definition.next().unwrap().unwrap();
    assert_eq!((key, value_store.as_bytes().unwrap()), ("value_store_id", vec![2; width].as_slice()));
    assert!(definition.next().is_none());
    let (key, scope) = root.next().unwrap().unwrap();
    assert_eq!((key, scope.as_bytes().unwrap()), ("scope_id", vec![1; width].as_slice()));
    assert!(root.next().is_none());
  }
}

#[test]
fn empty_containers_and_wrong_typed_access_are_explicit() {
  for bytes in [array(&[]), map(&[])] {
    let value = borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).unwrap();
    assert!(value.as_bytes().is_none());
    if bytes[0] == 9 {
      assert!(value.array_entries().unwrap().next().is_none());
      assert!(value.map_entries().is_err());
    } else {
      assert!(value.map_entries().unwrap().next().is_none());
      assert!(value.array_entries().is_err());
    }
  }
  let bytes = frame(8, &[]);
  let value = borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).unwrap();
  assert_eq!(value.as_bytes(), Some([].as_slice()));
  assert!(value.map_entries().is_err());
  assert!(value.array_entries().is_err());
}

#[test]
fn every_truncation_and_trailing_byte_is_rejected_before_a_view_escapes() {
  let bytes = map(&[("a", array(&[frame(8, b"x"), map(&[("nested", frame(1, &[]))])]))]);
  for end in 0..bytes.len() {
    assert!(borrow_canonical_value(&bytes[..end], CanonicalValueBounds::CONFIG).is_err(), "truncation at {end}");
  }
  let mut trailing = bytes;
  trailing.push(0);
  assert!(borrow_canonical_value(&trailing, CanonicalValueBounds::CONFIG).is_err());
}

#[test]
fn borrowed_validation_preserves_order_duplicate_utf8_count_and_depth_guards() {
  for bytes in [
    map(&[("b", frame(1, &[])), ("a", frame(1, &[]))]),
    map(&[("a", frame(1, &[])), ("a", frame(1, &[]))]),
    frame(7, &[0xff]),
    frame(0xff, &[]),
    frame(9, &2u32.to_le_bytes()),
    frame(10, &1u32.to_le_bytes()),
  ] {
    assert!(borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).is_err());
  }
  let mut bytes = frame(1, &[]);
  for _ in 0..32 {
    bytes = array(&[bytes]);
  }
  assert!(borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).is_ok());
  bytes = array(&[bytes]);
  assert!(borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).is_err());
}

#[test]
fn borrowed_validation_uses_the_callers_scalar_and_total_bounds() {
  let bytes = frame(8, &vec![1; 65537]);
  assert!(borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).is_err());
  let value = borrow_canonical_value(&bytes, CanonicalValueBounds::SOURCE_VALUE).unwrap();
  assert_eq!(value.as_bytes().unwrap().len(), 65537);
  let mut bounds = CanonicalValueBounds::SOURCE_VALUE;
  bounds.maximum_value_length = bytes.len() - 1;
  assert!(borrow_canonical_value(&bytes, bounds).is_err());
  let small_unsigned = frame(5, &1u64.to_le_bytes());
  assert!(borrow_canonical_value(&small_unsigned, CanonicalValueBounds::CONFIG).is_err());
  assert!(borrow_canonical_value(&small_unsigned, CanonicalValueBounds::SOURCE_VALUE).is_ok());
}

#[test]
fn borrowed_view_preserves_all_permanent_tags_from_independent_retained_fixtures() {
  for profile in ["blake3-256", "sha512"] {
    let bytes = std::fs::read(format!(
      "{}/spec/fixtures/v4/canonical-config-value-v1/config-{profile}-all-tags-valid.bin",
      env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let value = borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).unwrap();
    let entries = value.map_entries().unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(entries.iter().map(|(key, _)| *key).collect::<Vec<_>>(), ["array", "bytes", "f64", "i64", "map", "u64", "utf8"]);
    assert_eq!(entries.iter().map(|(_, value)| value.encoded_bytes()[0]).collect::<Vec<_>>(), [9, 8, 6, 4, 10, 5, 7]);
    assert_eq!(entries[1].1.as_bytes(), Some([0, 255].as_slice()));
    let array_tags = entries[0].1.array_entries().unwrap().map(|item| item.unwrap().encoded_bytes()[0]).collect::<Vec<_>>();
    assert_eq!(array_tags, [1, 2, 3]);
    let nested = entries[4].1.map_entries().unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!((nested[0].0, nested[0].1.encoded_bytes()), ("a", frame(7, b"alpha").as_slice()));
    assert_eq!((nested[1].0, nested[1].1.encoded_bytes()), ("z", frame(1, &[]).as_slice()));
  }
}

#[test]
fn borrowed_validation_rejects_scalar_key_count_and_framing_boundaries() {
  let mut invalid_key = map(&[("a", frame(1, &[]))]);
  invalid_key[13] = 0xff;
  let mut oversized_key = map(&[("a", frame(1, &[]))]);
  oversized_key[9..13].copy_from_slice(&u32::MAX.to_le_bytes());
  for bytes in [
    invalid_key,
    oversized_key,
    vec![8, 255, 255, 255, 255],
    frame(1, &[0]),
    frame(2, &[0]),
    frame(3, &[1]),
    frame(4, &[0; 7]),
    frame(5, &[0; 9]),
    frame(6, &f64::NAN.to_bits().to_le_bytes()),
    frame(6, &f64::INFINITY.to_bits().to_le_bytes()),
    frame(6, &(-0.0f64).to_bits().to_le_bytes()),
    frame(9, &65536u32.to_le_bytes()),
    frame(10, &65536u32.to_le_bytes()),
    frame(9, &[0, 0, 0, 0, 1, 0, 0, 0, 0]),
    frame(10, &[0, 0, 0, 0, 0]),
  ] {
    let failure = borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).unwrap_err();
    let existing = aeordb::engine::v4::config_value::validate_canonical_value(&bytes, CanonicalValueBounds::CONFIG).unwrap_err();
    assert_eq!(failure, existing, "borrowed entry changed the canonical validation error");
  }
  let bytes = map(&[("é", frame(7, "雪".as_bytes()))]);
  let mut bounds = CanonicalValueBounds::CONFIG;
  bounds.maximum_key_length = 2;
  let value = borrow_canonical_value(&bytes, bounds).unwrap();
  let (key, child) = value.map_entries().unwrap().next().unwrap().unwrap();
  assert_eq!(key, "é");
  assert_eq!(child.encoded_bytes(), frame(7, "雪".as_bytes()));
  assert!(child.as_bytes().is_none());
  assert!(child.map_entries().is_err());
  assert!(child.array_entries().is_err());
  bounds.maximum_key_length = 1;
  assert_eq!(borrow_canonical_value(&bytes, bounds).unwrap_err().code(), "config_map_key_oversize");
}
