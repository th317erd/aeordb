/// BLAKE3 hash, 32 bytes.
pub type ChunkHash = [u8; 32];

/// A content-addressed, immutable block of data identified by its BLAKE3 hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
  pub hash: ChunkHash,
  pub data: Vec<u8>,
}

impl Chunk {
  /// Create a new chunk from raw data, computing the BLAKE3 hash.
  pub fn new(data: Vec<u8>) -> Self {
    let hash = hash_data(&data);
    Self { hash, data }
  }

  /// Re-hash the data and verify it matches the stored hash.
  pub fn verify(&self) -> bool {
    hash_data(&self.data) == self.hash
  }
}

/// Compute the BLAKE3 hash of arbitrary data.
pub fn hash_data(data: &[u8]) -> ChunkHash {
  *blake3::hash(data).as_bytes()
}

/// Convert a chunk hash to a hex string.
pub fn chunk_hash_to_hex(hash: &ChunkHash) -> String {
  hex::encode(hash)
}

/// Parse a hex string into a chunk hash.
pub fn chunk_hash_from_hex(hex_string: &str) -> Result<ChunkHash, ChunkHashParseError> {
  let bytes = hex::decode(hex_string).map_err(|_| ChunkHashParseError::InvalidHex)?;
  if bytes.len() != 32 {
    return Err(ChunkHashParseError::WrongLength(bytes.len()));
  }
  let mut hash = [0u8; 32];
  hash.copy_from_slice(&bytes);
  Ok(hash)
}

#[derive(Debug, thiserror::Error)]
pub enum ChunkHashParseError {
  #[error("invalid hex string")]
  InvalidHex,

  #[error("expected 32 bytes, got {0}")]
  WrongLength(usize),
}
