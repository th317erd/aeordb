use crate::engine::errors::{EngineError, EngineResult};

#[derive(Debug, Clone, PartialEq)]
pub struct DeletionRecord {
  pub path: String,
  pub deleted_at: i64,
  pub reason: Option<String>,
}

impl DeletionRecord {
  pub fn new(path: String, reason: Option<String>) -> Self {
    let now = chrono::Utc::now().timestamp_millis();
    Self {
      path,
      deleted_at: now,
      reason,
    }
  }

  pub fn serialize(&self) -> Vec<u8> {
    let path_bytes = self.path.as_bytes();
    let reason_bytes = self.reason.as_deref().unwrap_or("").as_bytes();

    let capacity = 2 + path_bytes.len() + 8 + 2 + reason_bytes.len();
    let mut buffer = Vec::with_capacity(capacity);

    buffer.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
    buffer.extend_from_slice(path_bytes);

    buffer.extend_from_slice(&self.deleted_at.to_le_bytes());

    buffer.extend_from_slice(&(reason_bytes.len() as u16).to_le_bytes());
    buffer.extend_from_slice(reason_bytes);

    buffer
  }

  pub fn deserialize(data: &[u8]) -> EngineResult<Self> {
    let mut offset = 0;

    let path_length = read_u16(data, &mut offset)? as usize;
    let path = read_string(data, &mut offset, path_length)?;

    let deleted_at = read_i64(data, &mut offset)?;

    let reason_length = read_u16(data, &mut offset)? as usize;
    let reason = if reason_length == 0 {
      None
    } else {
      Some(read_string(data, &mut offset, reason_length)?)
    };

    Ok(Self {
      path,
      deleted_at,
      reason,
    })
  }
}

fn read_u16(data: &[u8], offset: &mut usize) -> EngineResult<u16> {
  if *offset + 2 > data.len() {
    return Err(EngineError::UnexpectedEof);
  }
  let value = u16::from_le_bytes([data[*offset], data[*offset + 1]]);
  *offset += 2;
  Ok(value)
}

fn read_i64(data: &[u8], offset: &mut usize) -> EngineResult<i64> {
  if *offset + 8 > data.len() {
    return Err(EngineError::UnexpectedEof);
  }
  let bytes: [u8; 8] = data[*offset..*offset + 8].try_into().unwrap();
  let value = i64::from_le_bytes(bytes);
  *offset += 8;
  Ok(value)
}

fn read_bytes(data: &[u8], offset: &mut usize, length: usize) -> EngineResult<Vec<u8>> {
  if *offset + length > data.len() {
    return Err(EngineError::UnexpectedEof);
  }
  let bytes = data[*offset..*offset + length].to_vec();
  *offset += length;
  Ok(bytes)
}

fn read_string(data: &[u8], offset: &mut usize, length: usize) -> EngineResult<String> {
  let bytes = read_bytes(data, offset, length)?;
  String::from_utf8(bytes).map_err(|error| EngineError::CorruptEntry {
    offset: *offset as u64,
    reason: format!("Invalid UTF-8 string: {}", error),
  })
}
