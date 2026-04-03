use crate::engine::errors::{EngineError, EngineResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CompressionAlgorithm {
  None = 0x00,
  Zstd = 0x01,
}

impl CompressionAlgorithm {
  pub fn from_u8(value: u8) -> Option<Self> {
    match value {
      0x00 => Some(CompressionAlgorithm::None),
      0x01 => Some(CompressionAlgorithm::Zstd),
      _ => None,
    }
  }

  pub fn to_u8(self) -> u8 {
    self as u8
  }
}

/// Compress data using the specified algorithm.
pub fn compress(data: &[u8], algorithm: CompressionAlgorithm) -> EngineResult<Vec<u8>> {
  match algorithm {
    CompressionAlgorithm::None => Ok(data.to_vec()),
    CompressionAlgorithm::Zstd => {
      zstd::encode_all(data, 1) // level 1 = fast
        .map_err(EngineError::IoError)
    }
  }
}

/// Decompress data using the specified algorithm.
pub fn decompress(data: &[u8], algorithm: CompressionAlgorithm) -> EngineResult<Vec<u8>> {
  match algorithm {
    CompressionAlgorithm::None => Ok(data.to_vec()),
    CompressionAlgorithm::Zstd => {
      zstd::decode_all(data)
        .map_err(EngineError::IoError)
    }
  }
}

/// Determine if data should be compressed based on content type and size.
pub fn should_compress(content_type: Option<&str>, data_size: usize) -> bool {
  // Don't compress small data
  if data_size < 500 {
    return false;
  }

  // Don't compress already-compressed formats
  if let Some(content_type_str) = content_type {
    let content_type_lower = content_type_str.to_lowercase();
    if content_type_lower.starts_with("image/jpeg")
      || content_type_lower.starts_with("image/png")
      || content_type_lower.starts_with("image/gif")
      || content_type_lower.starts_with("image/webp")
      || content_type_lower.starts_with("video/")
      || content_type_lower.starts_with("audio/")
      || content_type_lower.contains("zip")
      || content_type_lower.contains("gzip")
      || content_type_lower.contains("compressed")
      || content_type_lower.contains("zstd")
    {
      return false;
    }
  }

  true
}
