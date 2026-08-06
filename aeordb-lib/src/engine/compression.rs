use crate::engine::errors::{EngineError, EngineResult};
use std::io::Read;

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
    CompressionAlgorithm::Zstd => zstd::decode_all(data).map_err(EngineError::IoError),
  }
}

/// Decompress data without allowing its decoded form to exceed a caller-owned
/// allocation bound.
pub fn decompress_bounded(data: &[u8], algorithm: CompressionAlgorithm, maximum_output_length: usize) -> EngineResult<Vec<u8>> {
  match algorithm {
    CompressionAlgorithm::None => {
      if data.len() > maximum_output_length {
        return Err(EngineError::InvalidInput(format!(
          "decompressed payload length {} exceeds caller bound {}",
          data.len(),
          maximum_output_length
        )));
      }
      Ok(data.to_vec())
    }
    CompressionAlgorithm::Zstd => {
      let mut decoder = zstd::stream::read::Decoder::new(data).map_err(EngineError::IoError)?;
      let initial_capacity = data.len().min(maximum_output_length).min(64 * 1024);
      let mut output = Vec::with_capacity(initial_capacity);
      let mut buffer = [0u8; 16 * 1024];
      loop {
        let read = decoder.read(&mut buffer).map_err(EngineError::IoError)?;
        if read == 0 {
          break;
        }
        let decoded_length = output
          .len()
          .checked_add(read)
          .ok_or_else(|| EngineError::InvalidInput("decompressed payload length overflowed usize".to_string()))?;
        if decoded_length > maximum_output_length {
          return Err(EngineError::InvalidInput(format!("decompressed payload exceeds caller bound {}", maximum_output_length)));
        }
        output.extend_from_slice(&buffer[..read]);
      }
      Ok(output)
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
