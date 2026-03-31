use crate::engine::errors::{EngineError, EngineResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum HashAlgorithm {
  Blake3_256 = 0x0001,
  Sha256     = 0x0002,
  Sha512     = 0x0003,
  Sha3_256   = 0x0004,
  Sha3_512   = 0x0005,
}

impl HashAlgorithm {
  pub fn hash_length(&self) -> usize {
    match self {
      HashAlgorithm::Blake3_256 => 32,
      HashAlgorithm::Sha256     => 32,
      HashAlgorithm::Sha512     => 64,
      HashAlgorithm::Sha3_256   => 32,
      HashAlgorithm::Sha3_512   => 64,
    }
  }

  pub fn compute_hash(&self, data: &[u8]) -> EngineResult<Vec<u8>> {
    match self {
      HashAlgorithm::Blake3_256 => {
        let hash = blake3::hash(data);
        Ok(hash.as_bytes().to_vec())
      }
      other => Err(EngineError::InvalidHashAlgorithm(*other as u16)),
    }
  }

  pub fn from_u16(value: u16) -> Option<Self> {
    match value {
      0x0001 => Some(HashAlgorithm::Blake3_256),
      0x0002 => Some(HashAlgorithm::Sha256),
      0x0003 => Some(HashAlgorithm::Sha512),
      0x0004 => Some(HashAlgorithm::Sha3_256),
      0x0005 => Some(HashAlgorithm::Sha3_512),
      _      => None,
    }
  }

  pub fn to_u16(self) -> u16 {
    self as u16
  }
}
