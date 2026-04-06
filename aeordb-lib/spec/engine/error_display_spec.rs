use aeordb::engine::errors::EngineError;

#[test]
fn test_display_io_error() {
  let io_error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
  let engine_error = EngineError::IoError(io_error);
  let display_text = format!("{}", engine_error);

  assert!(
    display_text.contains("IO error"),
    "expected 'IO error' prefix, got: {}",
    display_text
  );
  assert!(
    display_text.contains("access denied"),
    "expected underlying message, got: {}",
    display_text
  );
}

#[test]
fn test_display_invalid_magic() {
  let engine_error = EngineError::InvalidMagic;
  let display_text = format!("{}", engine_error);

  assert_eq!(display_text, "Invalid magic bytes");
}

#[test]
fn test_display_invalid_entry_version() {
  let engine_error = EngineError::InvalidEntryVersion(42);
  let display_text = format!("{}", engine_error);

  assert_eq!(display_text, "Invalid entry version: 42");
}

#[test]
fn test_display_invalid_entry_type() {
  let engine_error = EngineError::InvalidEntryType(0xFF);
  let display_text = format!("{}", engine_error);

  assert_eq!(display_text, "Invalid entry type: 0xFF");
}

#[test]
fn test_display_invalid_entry_type_low_value() {
  let engine_error = EngineError::InvalidEntryType(0x03);
  let display_text = format!("{}", engine_error);

  assert_eq!(display_text, "Invalid entry type: 0x03");
}

#[test]
fn test_display_invalid_hash_algorithm() {
  let engine_error = EngineError::InvalidHashAlgorithm(0xBEEF);
  let display_text = format!("{}", engine_error);

  assert_eq!(display_text, "Invalid hash algorithm: 0xBEEF");
}

#[test]
fn test_display_invalid_hash_algorithm_low_value() {
  let engine_error = EngineError::InvalidHashAlgorithm(0x0001);
  let display_text = format!("{}", engine_error);

  assert_eq!(display_text, "Invalid hash algorithm: 0x0001");
}

#[test]
fn test_display_corrupt_entry() {
  let engine_error = EngineError::CorruptEntry {
    offset: 1024,
    reason: "checksum mismatch".to_string(),
  };
  let display_text = format!("{}", engine_error);

  assert_eq!(display_text, "Corrupt entry at offset 1024: checksum mismatch");
}

#[test]
fn test_display_corrupt_entry_zero_offset() {
  let engine_error = EngineError::CorruptEntry {
    offset: 0,
    reason: "truncated header".to_string(),
  };
  let display_text = format!("{}", engine_error);

  assert_eq!(display_text, "Corrupt entry at offset 0: truncated header");
}

#[test]
fn test_display_unexpected_eof() {
  let engine_error = EngineError::UnexpectedEof;
  let display_text = format!("{}", engine_error);

  assert_eq!(display_text, "Unexpected end of file");
}

#[test]
fn test_display_not_found() {
  let engine_error = EngineError::NotFound("/users/alice".to_string());
  let display_text = format!("{}", engine_error);

  assert_eq!(display_text, "Not found: /users/alice");
}

#[test]
fn test_display_already_exists() {
  let engine_error = EngineError::AlreadyExists("/data/config".to_string());
  let display_text = format!("{}", engine_error);

  assert_eq!(display_text, "Already exists: /data/config");
}

#[test]
fn test_display_range_query_not_supported() {
  let engine_error = EngineError::RangeQueryNotSupported("json_flatten".to_string());
  let display_text = format!("{}", engine_error);

  assert_eq!(
    display_text,
    "Range query not supported: converter 'json_flatten' is not order-preserving"
  );
}

#[test]
fn test_display_json_parse_error() {
  let engine_error = EngineError::JsonParseError("unexpected token at line 3".to_string());
  let display_text = format!("{}", engine_error);

  assert_eq!(display_text, "JSON parse error: unexpected token at line 3");
}

#[test]
fn test_display_patch_database() {
  let engine_error = EngineError::PatchDatabase("cannot open patch as standalone".to_string());
  let display_text = format!("{}", engine_error);

  assert_eq!(display_text, "Patch database: cannot open patch as standalone");
}

// --- Additional coverage for std::error::Error impl ---

#[test]
fn test_error_source_io_error() {
  use std::error::Error;

  let io_error = std::io::Error::new(std::io::ErrorKind::NotFound, "file gone");
  let engine_error = EngineError::IoError(io_error);

  assert!(
    engine_error.source().is_some(),
    "IoError variant should return a source"
  );
}

#[test]
fn test_error_source_non_io_variants_return_none() {
  use std::error::Error;

  let variants: Vec<EngineError> = vec![
    EngineError::InvalidMagic,
    EngineError::InvalidEntryVersion(1),
    EngineError::InvalidEntryType(1),
    EngineError::InvalidHashAlgorithm(1),
    EngineError::CorruptEntry { offset: 0, reason: "x".into() },
    EngineError::UnexpectedEof,
    EngineError::NotFound("x".into()),
    EngineError::AlreadyExists("x".into()),
    EngineError::RangeQueryNotSupported("x".into()),
    EngineError::JsonParseError("x".into()),
    EngineError::ReservedUserId,
    EngineError::UnsafeQueryField("x".into()),
    EngineError::PatchDatabase("x".into()),
  ];

  for variant in &variants {
    assert!(
      variant.source().is_none(),
      "non-IoError variant {:?} should return None for source()",
      variant
    );
  }
}

// --- From<std::io::Error> conversion tests ---

#[test]
fn test_from_io_unexpected_eof_becomes_unexpected_eof() {
  let io_error = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short read");
  let engine_error: EngineError = io_error.into();

  assert!(
    matches!(engine_error, EngineError::UnexpectedEof),
    "UnexpectedEof io error should convert to EngineError::UnexpectedEof, got: {:?}",
    engine_error
  );
}

#[test]
fn test_from_io_other_error_becomes_io_error() {
  let io_error = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe broke");
  let engine_error: EngineError = io_error.into();

  assert!(
    matches!(engine_error, EngineError::IoError(_)),
    "non-UnexpectedEof io error should convert to EngineError::IoError, got: {:?}",
    engine_error
  );
}
