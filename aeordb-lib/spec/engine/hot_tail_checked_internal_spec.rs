use std::io::{Cursor, Read, Seek, SeekFrom};

use super::*;

struct FailingReader;

impl Read for FailingReader {
  fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
    Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
  }
}

impl Seek for FailingReader {
  fn seek(&mut self, _position: SeekFrom) -> std::io::Result<u64> {
    Ok(0)
  }
}

#[test]
fn checked_hot_tail_reader_round_trips_valid_payloads() {
  let expected = HotTailPayload {
    writes: vec![KVEntry { hash: vec![0xAB; 32], type_flags: 3, offset: 4_096, total_length: 81 }],
    voids: vec![VoidRecord { offset: 8_192, size: 512 }],
  };
  let bytes = serialize_hot_tail(&expected, 32);

  let actual = read_hot_tail_checked(&mut Cursor::new(bytes), 0, 32).unwrap();

  assert_eq!(actual.writes.len(), 1);
  assert_eq!(actual.writes[0].hash, expected.writes[0].hash);
  assert_eq!(actual.writes[0].offset, expected.writes[0].offset);
  assert_eq!(actual.voids, expected.voids);
}

#[test]
fn checked_hot_tail_reader_distinguishes_structural_damage() {
  let mut wrong_magic = serialize_hot_tail(&HotTailPayload::default(), 32);
  wrong_magic[0] ^= 0xFF;
  assert!(matches!(read_hot_tail_checked(&mut Cursor::new(wrong_magic), 0, 32), Err(EngineError::InvalidMagic)));

  let mut wrong_version = serialize_hot_tail(&HotTailPayload::default(), 32);
  wrong_version[5] = 99;
  assert!(matches!(read_hot_tail_checked(&mut Cursor::new(wrong_version), 0, 32), Err(EngineError::InvalidEntryVersion(99))));

  let truncated = vec![0u8; HOT_TAIL_HEADER_SIZE - 1];
  assert!(matches!(read_hot_tail_checked(&mut Cursor::new(truncated), 0, 32), Err(EngineError::UnexpectedEof)));
}

#[test]
fn checked_hot_tail_reader_preserves_underlying_io_failures() {
  assert!(matches!(
    read_hot_tail_checked(&mut FailingReader, 0, 32),
    Err(EngineError::IoError(error)) if error.kind() == std::io::ErrorKind::PermissionDenied
  ));
}

#[test]
fn only_structural_hot_tail_failures_are_safe_to_rebuild_from_wal() {
  assert!(is_rebuildable_hot_tail_error(&EngineError::InvalidMagic));
  assert!(is_rebuildable_hot_tail_error(&EngineError::InvalidEntryVersion(7)));
  assert!(is_rebuildable_hot_tail_error(&EngineError::UnexpectedEof));
  assert!(is_rebuildable_hot_tail_error(&EngineError::CorruptEntry { offset: 1, reason: "torn".to_string() }));
  assert!(!is_rebuildable_hot_tail_error(&EngineError::IoError(std::io::Error::from(std::io::ErrorKind::PermissionDenied))));
  assert!(!is_rebuildable_hot_tail_error(&EngineError::ResourceExhausted("allocation".to_string())));
}
