use crate::engine::errors::{EngineError, EngineResult};

#[derive(Debug, Clone, PartialEq)]
pub struct ChildEntry {
  pub entry_type: u8,
  pub hash: Vec<u8>,
  pub total_size: u64,
  pub created_at: i64,
  pub updated_at: i64,
  pub name: String,
  pub content_type: Option<String>,
  pub virtual_time: u64,  // ordering key for conflict resolution
  pub node_id: u64,       // tiebreaker for deterministic ordering
}

impl ChildEntry {
  pub fn serialize(&self, hash_length: usize) -> Vec<u8> {
    let name_bytes = self.name.as_bytes();
    let content_type_bytes = self.content_type.as_deref().unwrap_or("").as_bytes();

    let capacity = 1 + hash_length + 8 + 8 + 8
      + 2 + name_bytes.len()
      + 2 + content_type_bytes.len()
      + 8 + 8;

    let mut buffer = Vec::with_capacity(capacity);

    buffer.push(self.entry_type);
    buffer.extend_from_slice(&self.hash);

    buffer.extend_from_slice(&self.total_size.to_le_bytes());
    buffer.extend_from_slice(&self.created_at.to_le_bytes());
    buffer.extend_from_slice(&self.updated_at.to_le_bytes());

    buffer.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    buffer.extend_from_slice(name_bytes);

    buffer.extend_from_slice(&(content_type_bytes.len() as u16).to_le_bytes());
    buffer.extend_from_slice(content_type_bytes);

    buffer.extend_from_slice(&self.virtual_time.to_le_bytes());
    buffer.extend_from_slice(&self.node_id.to_le_bytes());

    buffer
  }

  pub fn deserialize(data: &[u8], hash_length: usize) -> EngineResult<(ChildEntry, usize)> {
    let mut offset = 0;

    if offset >= data.len() {
      return Err(EngineError::UnexpectedEof);
    }
    let entry_type = data[offset];
    offset += 1;

    let hash = read_bytes(data, &mut offset, hash_length)?;

    let total_size = read_u64(data, &mut offset)?;
    let created_at = read_i64(data, &mut offset)?;
    let updated_at = read_i64(data, &mut offset)?;

    let name_length = read_u16(data, &mut offset)? as usize;
    let name = read_string(data, &mut offset, name_length)?;

    let content_type_length = read_u16(data, &mut offset)? as usize;
    let content_type = if content_type_length == 0 {
      None
    } else {
      Some(read_string(data, &mut offset, content_type_length)?)
    };

    let virtual_time = if offset + 8 <= data.len() {
      read_u64(data, &mut offset)?
    } else {
      0 // default for old entries without this field
    };

    let node_id = if offset + 8 <= data.len() {
      read_u64(data, &mut offset)?
    } else {
      0 // default for old entries without this field
    };

    let entry = ChildEntry {
      entry_type,
      hash,
      total_size,
      created_at,
      updated_at,
      name,
      content_type,
      virtual_time,
      node_id,
    };

    Ok((entry, offset))
  }
}

pub fn serialize_child_entries(entries: &[ChildEntry], hash_length: usize) -> Vec<u8> {
  let mut buffer = Vec::new();
  for entry in entries {
    buffer.extend_from_slice(&entry.serialize(hash_length));
  }
  buffer
}

pub fn deserialize_child_entries(
  data: &[u8],
  hash_length: usize,
) -> EngineResult<Vec<ChildEntry>> {
  // Currently only v0 format exists — dispatch directly
  deserialize_child_entries_v0(data, hash_length)
}

fn deserialize_child_entries_v0(
  data: &[u8],
  hash_length: usize,
) -> EngineResult<Vec<ChildEntry>> {
  let mut entries = Vec::new();
  let mut offset = 0;

  while offset < data.len() {
    let (entry, bytes_consumed) = ChildEntry::deserialize(&data[offset..], hash_length)?;
    entries.push(entry);
    offset += bytes_consumed;
  }

  Ok(entries)
}

fn read_u16(data: &[u8], offset: &mut usize) -> EngineResult<u16> {
  if *offset + 2 > data.len() {
    return Err(EngineError::UnexpectedEof);
  }
  let value = u16::from_le_bytes([data[*offset], data[*offset + 1]]);
  *offset += 2;
  Ok(value)
}

fn read_u64(data: &[u8], offset: &mut usize) -> EngineResult<u64> {
  if *offset + 8 > data.len() {
    return Err(EngineError::UnexpectedEof);
  }
  let bytes: [u8; 8] = data[*offset..*offset + 8].try_into().unwrap();
  let value = u64::from_le_bytes(bytes);
  *offset += 8;
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
