use std::fmt;

#[derive(Debug)]
pub enum EngineError {
  IoError(std::io::Error),
  InvalidMagic,
  InvalidEntryVersion(u8),
  InvalidEntryType(u8),
  InvalidHashAlgorithm(u16),
  CorruptEntry { offset: u64, reason: String },
  UnexpectedEof,
  NotFound(String),
  AlreadyExists(String),
}

impl fmt::Display for EngineError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      EngineError::IoError(error) => write!(formatter, "IO error: {}", error),
      EngineError::InvalidMagic => write!(formatter, "Invalid magic bytes"),
      EngineError::InvalidEntryVersion(version) => {
        write!(formatter, "Invalid entry version: {}", version)
      }
      EngineError::InvalidEntryType(entry_type) => {
        write!(formatter, "Invalid entry type: 0x{:02X}", entry_type)
      }
      EngineError::InvalidHashAlgorithm(algorithm) => {
        write!(formatter, "Invalid hash algorithm: 0x{:04X}", algorithm)
      }
      EngineError::CorruptEntry { offset, reason } => {
        write!(formatter, "Corrupt entry at offset {}: {}", offset, reason)
      }
      EngineError::UnexpectedEof => write!(formatter, "Unexpected end of file"),
      EngineError::NotFound(path) => write!(formatter, "Not found: {}", path),
      EngineError::AlreadyExists(path) => write!(formatter, "Already exists: {}", path),
    }
  }
}

impl std::error::Error for EngineError {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      EngineError::IoError(error) => Some(error),
      _ => None,
    }
  }
}

impl From<std::io::Error> for EngineError {
  fn from(error: std::io::Error) -> Self {
    if error.kind() == std::io::ErrorKind::UnexpectedEof {
      return EngineError::UnexpectedEof;
    }
    EngineError::IoError(error)
  }
}

pub type EngineResult<T> = Result<T, EngineError>;
