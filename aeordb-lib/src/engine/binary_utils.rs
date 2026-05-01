use crate::engine::errors::{EngineError, EngineResult};

pub fn read_u16(data: &[u8], offset: &mut usize) -> EngineResult<u16> {
  if *offset + 2 > data.len() {
    return Err(EngineError::UnexpectedEof);
  }
  let value = u16::from_le_bytes([data[*offset], data[*offset + 1]]);
  *offset += 2;
  Ok(value)
}

pub fn read_u32(data: &[u8], offset: &mut usize) -> EngineResult<u32> {
  if *offset + 4 > data.len() {
    return Err(EngineError::UnexpectedEof);
  }
  let value = u32::from_le_bytes([
    data[*offset],
    data[*offset + 1],
    data[*offset + 2],
    data[*offset + 3],
  ]);
  *offset += 4;
  Ok(value)
}

pub fn read_u64(data: &[u8], offset: &mut usize) -> EngineResult<u64> {
  if *offset + 8 > data.len() {
    return Err(EngineError::UnexpectedEof);
  }
  let bytes: [u8; 8] = data[*offset..*offset + 8].try_into().unwrap();
  let value = u64::from_le_bytes(bytes);
  *offset += 8;
  Ok(value)
}

pub fn read_i64(data: &[u8], offset: &mut usize) -> EngineResult<i64> {
  if *offset + 8 > data.len() {
    return Err(EngineError::UnexpectedEof);
  }
  let bytes: [u8; 8] = data[*offset..*offset + 8].try_into().unwrap();
  let value = i64::from_le_bytes(bytes);
  *offset += 8;
  Ok(value)
}

pub fn read_bytes(data: &[u8], offset: &mut usize, length: usize) -> EngineResult<Vec<u8>> {
  if *offset + length > data.len() {
    return Err(EngineError::UnexpectedEof);
  }
  let bytes = data[*offset..*offset + length].to_vec();
  *offset += length;
  Ok(bytes)
}

pub fn read_string(data: &[u8], offset: &mut usize, length: usize) -> EngineResult<String> {
  let bytes = read_bytes(data, offset, length)?;
  String::from_utf8(bytes).map_err(|error| EngineError::CorruptEntry {
    offset: *offset as u64,
    reason: format!("Invalid UTF-8 string: {}", error),
  })
}
