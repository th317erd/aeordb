use crate::engine::errors::{EngineError, EngineResult};

pub fn read_u16(data: &[u8], offset: &mut usize) -> EngineResult<u16> {
  Ok(u16::from_le_bytes(read_array(data, offset)?))
}

pub fn read_u32(data: &[u8], offset: &mut usize) -> EngineResult<u32> {
  Ok(u32::from_le_bytes(read_array(data, offset)?))
}

pub fn read_u64(data: &[u8], offset: &mut usize) -> EngineResult<u64> {
  Ok(u64::from_le_bytes(read_array(data, offset)?))
}

pub fn read_i64(data: &[u8], offset: &mut usize) -> EngineResult<i64> {
  Ok(i64::from_le_bytes(read_array(data, offset)?))
}

pub fn read_bytes(data: &[u8], offset: &mut usize, length: usize) -> EngineResult<Vec<u8>> {
  Ok(read_slice(data, offset, length)?.to_vec())
}

pub fn read_string(data: &[u8], offset: &mut usize, length: usize) -> EngineResult<String> {
  let bytes = read_bytes(data, offset, length)?;
  String::from_utf8(bytes)
    .map_err(|error| EngineError::CorruptEntry { offset: *offset as u64, reason: format!("Invalid UTF-8 string: {}", error) })
}

fn read_array<const LENGTH: usize>(data: &[u8], offset: &mut usize) -> EngineResult<[u8; LENGTH]> {
  let mut output = [0_u8; LENGTH];
  output.copy_from_slice(read_slice(data, offset, LENGTH)?);
  Ok(output)
}

fn read_slice<'a>(data: &'a [u8], offset: &mut usize, length: usize) -> EngineResult<&'a [u8]> {
  let end = offset.checked_add(length).filter(|end| *end <= data.len()).ok_or(EngineError::UnexpectedEof)?;
  let bytes = &data[*offset..end];
  *offset = end;
  Ok(bytes)
}
