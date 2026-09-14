use super::*;

#[test]
fn defensive_array_cursor_errors_fuse_even_with_a_broken_private_invariant() {
  // Public construction rejects both values. Deliberately bypass it to prove
  // iterator error behavior if future internal code violates that invariant.
  for (bytes, first_succeeds) in
    [(vec![9, 9, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 0], true), (vec![9, 14, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0], false)]
  {
    assert!(borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).is_err());
    let mut entries = BorrowedCanonicalValueV1 { bytes: &bytes }.array_entries().unwrap();
    match entries.next().unwrap() {
      Ok(value) => {
        assert!(first_succeeds);
        assert_eq!(value.encoded_bytes(), [1, 0, 0, 0, 0]);
        assert_eq!(entries.next().unwrap().unwrap_err().code(), "truncated_input");
      }
      Err(error) => {
        assert!(!first_succeeds);
        assert_eq!(error.code(), "trailing_bytes");
      }
    }
    assert!(entries.next().is_none());
    assert!(entries.next().is_none());
  }
}

#[test]
fn defensive_map_cursor_propagates_key_errors_once() {
  for (bytes, code) in [
    (vec![10, 10, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 255, 1, 0, 0, 0, 0], "config_map_key_utf8"),
    (vec![10, 4, 0, 0, 0, 1, 0, 0, 0], "truncated_input"),
    (vec![10, 8, 0, 0, 0, 1, 0, 0, 0, 255, 255, 255, 255], "truncated_input"),
  ] {
    assert!(borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).is_err());
    let mut entries = BorrowedCanonicalValueV1 { bytes: &bytes }.map_entries().unwrap();
    assert_eq!(entries.next().unwrap().unwrap_err().code(), code);
    assert!(entries.next().is_none());
    assert!(entries.next().is_none());
  }
}
