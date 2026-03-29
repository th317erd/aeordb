use chrono::Utc;
use thiserror::Error;

/// Size of the chunk header in bytes.
pub const HEADER_SIZE: usize = 33;

#[derive(Debug, Error)]
pub enum ChunkHeaderError {
  #[error("header data too short: expected {HEADER_SIZE} bytes, got {0}")]
  TooShort(usize),

  #[error("unsupported format version: {0}")]
  UnsupportedVersion(u8),
}

/// Metadata header prepended to every stored chunk.
///
/// Layout (33 bytes total):
///   [0]       format_version  (u8)
///   [1..9]    created_at      (i64, big-endian, millis since epoch)
///   [9..17]   updated_at      (i64, big-endian, millis since epoch)
///   [17..33]  reserved        ([u8; 16], zeros)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkHeader {
  pub format_version: u8,
  pub created_at: i64,
  pub updated_at: i64,
  pub reserved: [u8; 16],
}

impl ChunkHeader {
  /// Create a new header with format_version=1, current timestamps, zero reserved.
  pub fn new() -> Self {
    let now = Utc::now().timestamp_millis();
    Self {
      format_version: 1,
      created_at: now,
      updated_at: now,
      reserved: [0u8; 16],
    }
  }

  /// Serialize the header into a fixed-size byte array.
  pub fn serialize(&self) -> [u8; HEADER_SIZE] {
    let mut buffer = [0u8; HEADER_SIZE];
    buffer[0] = self.format_version;
    buffer[1..9].copy_from_slice(&self.created_at.to_be_bytes());
    buffer[9..17].copy_from_slice(&self.updated_at.to_be_bytes());
    buffer[17..33].copy_from_slice(&self.reserved);
    buffer
  }

  /// Deserialize a header from a byte slice of exactly HEADER_SIZE bytes.
  pub fn deserialize(bytes: &[u8; HEADER_SIZE]) -> Result<Self, ChunkHeaderError> {
    let format_version = bytes[0];
    if format_version == 0 {
      return Err(ChunkHeaderError::UnsupportedVersion(format_version));
    }

    let created_at = i64::from_be_bytes(
      bytes[1..9].try_into().unwrap(),
    );
    let updated_at = i64::from_be_bytes(
      bytes[9..17].try_into().unwrap(),
    );
    let mut reserved = [0u8; 16];
    reserved.copy_from_slice(&bytes[17..33]);

    Ok(Self {
      format_version,
      created_at,
      updated_at,
      reserved,
    })
  }

  /// Deserialize from a slice, validating length first.
  pub fn deserialize_from_slice(bytes: &[u8]) -> Result<Self, ChunkHeaderError> {
    if bytes.len() < HEADER_SIZE {
      return Err(ChunkHeaderError::TooShort(bytes.len()));
    }
    let header_bytes: &[u8; HEADER_SIZE] = bytes[..HEADER_SIZE].try_into().unwrap();
    Self::deserialize(header_bytes)
  }
}

impl Default for ChunkHeader {
  fn default() -> Self {
    Self::new()
  }
}
