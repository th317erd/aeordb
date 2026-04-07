use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;

pub const FILE_HEADER_SIZE: usize = 256;
pub const FILE_MAGIC: &[u8; 4] = b"AEOR";

#[derive(Debug, Clone)]
pub struct FileHeader {
  pub header_version: u8,
  pub hash_algo: HashAlgorithm,
  pub created_at: i64,
  pub updated_at: i64,
  pub kv_block_offset: u64,
  pub kv_block_length: u64,
  pub kv_block_version: u8,
  pub nvt_offset: u64,
  pub nvt_length: u64,
  pub nvt_version: u8,
  pub head_hash: Vec<u8>,
  pub entry_count: u64,
  pub resize_in_progress: bool,
  pub buffer_kvs_offset: u64,
  pub buffer_nvt_offset: u64,
  pub backup_type: u8,        // 0=normal, 1=full export, 2=patch
  pub base_hash: Vec<u8>,     // source version hash
  pub target_hash: Vec<u8>,   // destination version hash
}

impl FileHeader {
  pub fn new(hash_algo: HashAlgorithm) -> Self {
    let now = chrono::Utc::now().timestamp_millis();

    let hash_length = hash_algo.hash_length();

    FileHeader {
      header_version: 1,
      hash_algo,
      created_at: now,
      updated_at: now,
      kv_block_offset: 0,
      kv_block_length: 0,
      kv_block_version: 1,
      nvt_offset: 0,
      nvt_length: 0,
      nvt_version: 1,
      head_hash: vec![0u8; hash_length],
      entry_count: 0,
      resize_in_progress: false,
      buffer_kvs_offset: 0,
      buffer_nvt_offset: 0,
      backup_type: 0,
      base_hash: vec![0u8; hash_length],
      target_hash: vec![0u8; hash_length],
    }
  }

  pub fn serialize(&self) -> [u8; FILE_HEADER_SIZE] {
    let mut buffer = [0u8; FILE_HEADER_SIZE];
    let mut offset = 0;

    // magic: 4 bytes
    buffer[offset..offset + 4].copy_from_slice(FILE_MAGIC);
    offset += 4;

    // header_version: 1 byte
    buffer[offset] = self.header_version;
    offset += 1;

    // hash_algo: 2 bytes
    buffer[offset..offset + 2].copy_from_slice(&self.hash_algo.to_u16().to_le_bytes());
    offset += 2;

    // created_at: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.created_at.to_le_bytes());
    offset += 8;

    // updated_at: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.updated_at.to_le_bytes());
    offset += 8;

    // kv_block_offset: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.kv_block_offset.to_le_bytes());
    offset += 8;

    // kv_block_length: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.kv_block_length.to_le_bytes());
    offset += 8;

    // kv_block_version: 1 byte
    buffer[offset] = self.kv_block_version;
    offset += 1;

    // nvt_offset: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.nvt_offset.to_le_bytes());
    offset += 8;

    // nvt_length: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.nvt_length.to_le_bytes());
    offset += 8;

    // nvt_version: 1 byte
    buffer[offset] = self.nvt_version;
    offset += 1;

    // head_hash: dynamic length (hash_algo.hash_length() bytes)
    let hash_length = self.hash_algo.hash_length();
    let copy_length = hash_length.min(self.head_hash.len());
    buffer[offset..offset + copy_length].copy_from_slice(&self.head_hash[..copy_length]);
    offset += hash_length;

    // entry_count: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.entry_count.to_le_bytes());
    offset += 8;

    // resize_in_progress: 1 byte
    buffer[offset] = if self.resize_in_progress { 1 } else { 0 };
    offset += 1;

    // buffer_kvs_offset: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.buffer_kvs_offset.to_le_bytes());
    offset += 8;

    // buffer_nvt_offset: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.buffer_nvt_offset.to_le_bytes());
    offset += 8;

    // backup_type: 1 byte
    buffer[offset] = self.backup_type;
    offset += 1;

    // base_hash: hash_length bytes
    let copy_len = hash_length.min(self.base_hash.len());
    buffer[offset..offset + copy_len].copy_from_slice(&self.base_hash[..copy_len]);
    offset += hash_length;

    // target_hash: hash_length bytes
    let copy_len = hash_length.min(self.target_hash.len());
    buffer[offset..offset + copy_len].copy_from_slice(&self.target_hash[..copy_len]);
    let _ = offset + hash_length; // suppress unused warning

    buffer
  }

  pub fn deserialize(bytes: &[u8; FILE_HEADER_SIZE]) -> EngineResult<Self> {
    let mut offset = 0;

    // magic: 4 bytes
    if &bytes[offset..offset + 4] != FILE_MAGIC {
      return Err(EngineError::InvalidMagic);
    }
    offset += 4;

    // header_version: 1 byte
    let header_version = bytes[offset];
    offset += 1;

    // hash_algo: 2 bytes
    let hash_algo_raw = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
    let hash_algo = HashAlgorithm::from_u16(hash_algo_raw)
      .ok_or(EngineError::InvalidHashAlgorithm(hash_algo_raw))?;
    offset += 2;

    // created_at: 8 bytes
    let created_at = i64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // updated_at: 8 bytes
    let updated_at = i64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // kv_block_offset: 8 bytes
    let kv_block_offset = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // kv_block_length: 8 bytes
    let kv_block_length = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // kv_block_version: 1 byte
    let kv_block_version = bytes[offset];
    offset += 1;

    // nvt_offset: 8 bytes
    let nvt_offset = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // nvt_length: 8 bytes
    let nvt_length = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // nvt_version: 1 byte
    let nvt_version = bytes[offset];
    offset += 1;

    // head_hash: dynamic length
    let hash_length = hash_algo.hash_length();
    let head_hash = bytes[offset..offset + hash_length].to_vec();
    offset += hash_length;

    // entry_count: 8 bytes
    let entry_count = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // resize_in_progress: 1 byte
    let resize_in_progress = bytes[offset] != 0;
    offset += 1;

    // buffer_kvs_offset: 8 bytes
    let buffer_kvs_offset = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // buffer_nvt_offset: 8 bytes
    let buffer_nvt_offset = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // backup_type: 1 byte
    let backup_type = bytes[offset];
    offset += 1;

    // base_hash: hash_length bytes
    let base_hash = bytes[offset..offset + hash_length].to_vec();
    offset += hash_length;

    // target_hash: hash_length bytes
    let target_hash = bytes[offset..offset + hash_length].to_vec();
    let _ = offset + hash_length; // suppress unused warning

    Ok(FileHeader {
      header_version,
      hash_algo,
      created_at,
      updated_at,
      kv_block_offset,
      kv_block_length,
      kv_block_version,
      nvt_offset,
      nvt_length,
      nvt_version,
      head_hash,
      entry_count,
      resize_in_progress,
      buffer_kvs_offset,
      buffer_nvt_offset,
      backup_type,
      base_hash,
      target_hash,
    })
  }
}
