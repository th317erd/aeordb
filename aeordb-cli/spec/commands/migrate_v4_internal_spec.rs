use super::{parse_byte_quantity, parse_source_commit};

#[test]
fn migration_byte_bounds_accept_only_canonical_binary_quantities() {
  assert_eq!(parse_byte_quantity("0").unwrap(), 0);
  assert_eq!(parse_byte_quantity("4096").unwrap(), 4096);
  assert_eq!(parse_byte_quantity("2KiB").unwrap(), 2 * 1024);
  assert_eq!(parse_byte_quantity("3MiB").unwrap(), 3 * 1024 * 1024);
  assert_eq!(parse_byte_quantity("4GiB").unwrap(), 4 * 1024 * 1024 * 1024);
  assert_eq!(parse_byte_quantity("1TiB").unwrap(), 1024_u64.pow(4));

  for invalid in ["", "1B", " 1", "1 ", "+1", "-1", "1.0", "1KB", "1mib", "MiB", "18446744073709551616", "16777216TiB"] {
    assert!(parse_byte_quantity(invalid).is_err(), "accepted invalid byte bound {invalid:?}");
  }
}

#[test]
fn migration_source_commit_requires_one_nonzero_sha1_width_value() {
  assert_eq!(parse_source_commit("A1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1").unwrap(), [0xa1; 20],);
  for invalid in [
    "",
    "00",
    "0000000000000000000000000000000000000000",
    "21212121212121212121212121212121212121",
    "212121212121212121212121212121212121212100",
    "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
  ] {
    assert!(parse_source_commit(invalid).is_err(), "accepted invalid source commit {invalid:?}");
  }
}
