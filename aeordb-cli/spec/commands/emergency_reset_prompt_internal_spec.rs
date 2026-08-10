use std::io::{self, BufRead, Cursor, Write};

use super::confirm_emergency_reset_with;

struct FlushFailure;

impl Write for FlushFailure {
  fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
    Ok(buffer.len())
  }

  fn flush(&mut self) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::BrokenPipe))
  }
}

struct ReadFailure;

impl io::Read for ReadFailure {
  fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
    Err(io::Error::from(io::ErrorKind::ConnectionReset))
  }
}

impl BufRead for ReadFailure {
  fn fill_buf(&mut self) -> io::Result<&[u8]> {
    Err(io::Error::from(io::ErrorKind::ConnectionReset))
  }

  fn consume(&mut self, _amount: usize) {}
}

#[test]
fn emergency_reset_prompt_propagates_terminal_failures() {
  let mut input = Cursor::new(b"y\n".to_vec());
  assert_eq!(confirm_emergency_reset_with(&mut input, &mut FlushFailure).unwrap_err().kind(), io::ErrorKind::BrokenPipe);

  let mut output = Vec::new();
  assert_eq!(confirm_emergency_reset_with(&mut ReadFailure, &mut output).unwrap_err().kind(), io::ErrorKind::ConnectionReset);
}

#[test]
fn emergency_reset_prompt_accepts_only_an_explicit_y() {
  for answer in ["y\n", "Y\n", " y \n"] {
    let mut input = Cursor::new(answer.as_bytes());
    let mut output = Vec::new();
    assert!(confirm_emergency_reset_with(&mut input, &mut output).unwrap());
    assert_eq!(output, b"Proceed? [y/N]: ");
  }

  for answer in ["\n", "yes\n", "n\n"] {
    let mut input = Cursor::new(answer.as_bytes());
    let mut output = Vec::new();
    assert!(!confirm_emergency_reset_with(&mut input, &mut output).unwrap());
  }
}
