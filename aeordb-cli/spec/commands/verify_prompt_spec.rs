use std::io::{self, BufRead, Cursor, Write};

use super::confirm_emergency_spill_replay_with;

struct FlushFailWriter;

impl Write for FlushFailWriter {
  fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
    Ok(buffer.len())
  }

  fn flush(&mut self) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::BrokenPipe))
  }
}

struct ReadFailReader;

impl io::Read for ReadFailReader {
  fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
    Err(io::Error::from(io::ErrorKind::ConnectionReset))
  }
}

impl BufRead for ReadFailReader {
  fn fill_buf(&mut self) -> io::Result<&[u8]> {
    Err(io::Error::from(io::ErrorKind::ConnectionReset))
  }

  fn consume(&mut self, _amount: usize) {}
}

#[test]
fn repair_prompt_propagates_flush_and_input_failures() {
  let mut yes = Cursor::new(b"yes\n".to_vec());
  let flush_error = confirm_emergency_spill_replay_with(&mut yes, &mut FlushFailWriter).unwrap_err();
  assert_eq!(flush_error.kind(), io::ErrorKind::BrokenPipe);

  let mut output = Vec::new();
  let read_error = confirm_emergency_spill_replay_with(&mut ReadFailReader, &mut output).unwrap_err();
  assert_eq!(read_error.kind(), io::ErrorKind::ConnectionReset);
}

#[test]
fn repair_prompt_accepts_only_explicit_yes_answers() {
  for answer in ["y\n", "Y\n", "yes\n", " YES \n"] {
    let mut input = Cursor::new(answer.as_bytes());
    let mut output = Vec::new();
    assert!(confirm_emergency_spill_replay_with(&mut input, &mut output).unwrap());
    assert_eq!(output, b"Proceed with emergency spill replay and repair? [y/N] ");
  }

  for answer in ["\n", "n\n", "no\n", "anything-else\n"] {
    let mut input = Cursor::new(answer.as_bytes());
    let mut output = Vec::new();
    assert!(!confirm_emergency_spill_replay_with(&mut input, &mut output).unwrap());
  }
}
