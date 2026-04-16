use std::io::Read;

use crate::engine::compression::CompressionAlgorithm;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;

pub const ENTRY_MAGIC: u32 = 0x0AE012DB;
pub const CURRENT_ENTRY_VERSION: u8 = 0;

#[derive(Debug, Clone)]
pub struct EntryHeader {
  pub entry_version: u8,
  pub entry_type: EntryType,
  pub flags: u8,
  pub hash_algo: HashAlgorithm,
  pub compression_algo: CompressionAlgorithm,
  pub encryption_algo: u8,
  pub key_length: u32,
  pub value_length: u32,
  pub timestamp: i64,
  pub total_length: u32,
  pub hash: Vec<u8>,
}

impl EntryHeader {
  /// Fixed portion size: magic(4) + entry_version(1) + entry_type(1) + flags(1)
  /// + hash_algo(2) + compression_algo(1) + encryption_algo(1)
  /// + key_length(4) + value_length(4) + timestamp(8) + total_length(4) = 31
  const FIXED_HEADER_SIZE: usize = 31;

  pub fn header_size(&self) -> usize {
    Self::FIXED_HEADER_SIZE + self.hash_algo.hash_length()
  }

  pub fn compute_total_length(
    hash_algo: HashAlgorithm,
    key_length: u32,
    value_length: u32,
  ) -> u32 {
    let header_size = Self::FIXED_HEADER_SIZE + hash_algo.hash_length();
    (header_size as u32) + key_length + value_length
  }

  pub fn compute_hash(
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
    algorithm: HashAlgorithm,
  ) -> EngineResult<Vec<u8>> {
    let mut hash_input = Vec::with_capacity(1 + key.len() + value.len());
    hash_input.push(entry_type.to_u8());
    hash_input.extend_from_slice(key);
    hash_input.extend_from_slice(value);
    algorithm.compute_hash(&hash_input)
  }

  pub fn verify(&self, key: &[u8], value: &[u8]) -> bool {
    let computed = match Self::compute_hash(self.entry_type, key, value, self.hash_algo) {
      Ok(hash) => hash,
      Err(_) => return false,
    };
    computed == self.hash
  }

  pub fn serialize(&self) -> Vec<u8> {
    let total_size = self.header_size();
    let mut buffer = Vec::with_capacity(total_size);

    buffer.extend_from_slice(&ENTRY_MAGIC.to_le_bytes());
    buffer.push(self.entry_version);
    buffer.push(self.entry_type.to_u8());
    buffer.push(self.flags);
    buffer.extend_from_slice(&self.hash_algo.to_u16().to_le_bytes());
    buffer.push(self.compression_algo.to_u8());
    buffer.push(self.encryption_algo);
    buffer.extend_from_slice(&self.key_length.to_le_bytes());
    buffer.extend_from_slice(&self.value_length.to_le_bytes());
    buffer.extend_from_slice(&self.timestamp.to_le_bytes());
    buffer.extend_from_slice(&self.total_length.to_le_bytes());
    buffer.extend_from_slice(&self.hash);

    buffer
  }

  pub fn deserialize(reader: &mut impl Read) -> EngineResult<EntryHeader> {
    let mut fixed_buffer = [0u8; Self::FIXED_HEADER_SIZE];
    reader.read_exact(&mut fixed_buffer)?;

    let magic = u32::from_le_bytes([
      fixed_buffer[0],
      fixed_buffer[1],
      fixed_buffer[2],
      fixed_buffer[3],
    ]);
    if magic != ENTRY_MAGIC {
      return Err(EngineError::InvalidMagic);
    }

    let entry_version = fixed_buffer[4];

    let entry_type = EntryType::from_u8(fixed_buffer[5])?;
    let flags = fixed_buffer[6];

    let hash_algo_raw = u16::from_le_bytes([fixed_buffer[7], fixed_buffer[8]]);
    let hash_algo = HashAlgorithm::from_u16(hash_algo_raw)
      .ok_or(EngineError::InvalidHashAlgorithm(hash_algo_raw))?;

    let compression_algo_raw = fixed_buffer[9];
    let compression_algo = CompressionAlgorithm::from_u8(compression_algo_raw)
      .ok_or(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("Invalid compression algorithm: 0x{:02X}", compression_algo_raw),
      })?;

    let encryption_algo = fixed_buffer[10];

    let key_length = u32::from_le_bytes([
      fixed_buffer[11],
      fixed_buffer[12],
      fixed_buffer[13],
      fixed_buffer[14],
    ]);
    let value_length = u32::from_le_bytes([
      fixed_buffer[15],
      fixed_buffer[16],
      fixed_buffer[17],
      fixed_buffer[18],
    ]);
    let timestamp = i64::from_le_bytes([
      fixed_buffer[19],
      fixed_buffer[20],
      fixed_buffer[21],
      fixed_buffer[22],
      fixed_buffer[23],
      fixed_buffer[24],
      fixed_buffer[25],
      fixed_buffer[26],
    ]);
    let total_length = u32::from_le_bytes([
      fixed_buffer[27],
      fixed_buffer[28],
      fixed_buffer[29],
      fixed_buffer[30],
    ]);

    let hash_length = hash_algo.hash_length();
    let mut hash = vec![0u8; hash_length];
    reader.read_exact(&mut hash)?;

    Ok(EntryHeader {
      entry_version,
      entry_type,
      flags,
      hash_algo,
      compression_algo,
      encryption_algo,
      key_length,
      value_length,
      timestamp,
      total_length,
      hash,
    })
  }
}
