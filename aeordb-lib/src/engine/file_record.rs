use crate::engine::binary_utils::{read_u16, read_u32, read_u64, read_i64, read_bytes, read_string};
use crate::engine::errors::{EngineError, EngineResult};

#[derive(Debug, Clone, PartialEq)]
pub struct FileRecord {
  pub path: String,
  pub content_type: Option<String>,
  pub total_size: u64,
  pub created_at: i64,
  pub updated_at: i64,
  pub metadata: Vec<u8>,
  pub chunk_hashes: Vec<Vec<u8>>,
}

impl FileRecord {
  pub fn new(
    path: String,
    content_type: Option<String>,
    total_size: u64,
    chunk_hashes: Vec<Vec<u8>>,
  ) -> Self {
    let now = chrono::Utc::now().timestamp_millis();
    Self {
      path,
      content_type,
      total_size,
      created_at: now,
      updated_at: now,
      metadata: Vec::new(),
      chunk_hashes,
    }
  }

  pub fn serialize(&self, hash_length: usize) -> EngineResult<Vec<u8>> {
    let path_bytes = self.path.as_bytes();
    let content_type_bytes = self.content_type.as_deref().unwrap_or("").as_bytes();

    if path_bytes.len() > u16::MAX as usize {
      return Err(EngineError::InvalidInput(
        format!("Path too long: {} bytes exceeds u16 max (65535)", path_bytes.len()),
      ));
    }
    if content_type_bytes.len() > u16::MAX as usize {
      return Err(EngineError::InvalidInput(
        format!("Content type too long: {} bytes exceeds u16 max (65535)", content_type_bytes.len()),
      ));
    }
    if self.metadata.len() > u32::MAX as usize {
      return Err(EngineError::InvalidInput(
        format!("Metadata too large: {} bytes exceeds u32 max", self.metadata.len()),
      ));
    }
    if self.chunk_hashes.len() > u32::MAX as usize {
      return Err(EngineError::InvalidInput(
        format!("Too many chunk hashes: {} exceeds u32 max", self.chunk_hashes.len()),
      ));
    }

    let capacity = 2 + path_bytes.len()
      + 2 + content_type_bytes.len()
      + 8 + 8 + 8
      + 4 + self.metadata.len()
      + 4 + self.chunk_hashes.len() * hash_length;

    let mut buffer = Vec::with_capacity(capacity);

    buffer.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
    buffer.extend_from_slice(path_bytes);

    buffer.extend_from_slice(&(content_type_bytes.len() as u16).to_le_bytes());
    buffer.extend_from_slice(content_type_bytes);

    buffer.extend_from_slice(&self.total_size.to_le_bytes());
    buffer.extend_from_slice(&self.created_at.to_le_bytes());
    buffer.extend_from_slice(&self.updated_at.to_le_bytes());

    buffer.extend_from_slice(&(self.metadata.len() as u32).to_le_bytes());
    buffer.extend_from_slice(&self.metadata);

    buffer.extend_from_slice(&(self.chunk_hashes.len() as u32).to_le_bytes());
    for hash in &self.chunk_hashes {
      buffer.extend_from_slice(hash);
    }

    Ok(buffer)
  }

  pub fn deserialize(data: &[u8], hash_length: usize, version: u8) -> EngineResult<Self> {
    match version {
      0 => Self::deserialize_v0(data, hash_length),
      _ => Err(crate::engine::errors::EngineError::InvalidEntryVersion(version)),
    }
  }

  fn deserialize_v0(data: &[u8], hash_length: usize) -> EngineResult<Self> {
    let mut offset = 0;

    let path_length = read_u16(data, &mut offset)? as usize;
    let path = read_string(data, &mut offset, path_length)?;

    let content_type_length = read_u16(data, &mut offset)? as usize;
    let content_type = if content_type_length == 0 {
      None
    } else {
      Some(read_string(data, &mut offset, content_type_length)?)
    };

    let total_size = read_u64(data, &mut offset)?;
    let created_at = read_i64(data, &mut offset)?;
    let updated_at = read_i64(data, &mut offset)?;

    let metadata_length = read_u32(data, &mut offset)? as usize;
    let metadata = read_bytes(data, &mut offset, metadata_length)?;

    let chunk_count = read_u32(data, &mut offset)? as usize;
    let mut chunk_hashes = Vec::with_capacity(chunk_count);
    for _ in 0..chunk_count {
      let hash = read_bytes(data, &mut offset, hash_length)?;
      chunk_hashes.push(hash);
    }

    Ok(Self {
      path,
      content_type,
      total_size,
      created_at,
      updated_at,
      metadata,
      chunk_hashes,
    })
  }
}

