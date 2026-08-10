use std::io::{self, Read};
use std::path::Path;

use super::read_frame;
use crate::engine::HashAlgorithm;

struct InvalidReadCount;

impl Read for InvalidReadCount {
  fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
    Ok(2)
  }
}

#[test]
fn invalid_reader_byte_count_is_a_typed_workspace_error() {
  let error = read_frame(&mut InvalidReadCount, Path::new("invalid-reader"), HashAlgorithm::Blake3_256.hash_length()).unwrap_err();

  assert!(error.to_string().contains("invalid byte count"));
}

#[test]
fn production_repair_workspace_contains_no_panic_assumptions() {
  let source = include_str!("../../src/engine/directory_repair_workspace.rs");
  let production = source.split("\n#[cfg(test)]").next().unwrap_or(source);

  assert!(!production.contains("unreachable!("));
  assert!(!production.contains(".expect("));
}
