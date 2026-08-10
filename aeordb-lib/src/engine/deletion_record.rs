use crate::engine::binary_utils::{read_i64, read_string, read_u16};
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
    Self { path, deleted_at: now, reason }
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

  /// Deserialize a deletion record. Dispatches on the surrounding KV
  /// `EntryHeader.entry_version` — callers MUST pass it through. Future
  /// format changes add new arms here.
  pub fn deserialize(data: &[u8], version: u8) -> EngineResult<Self> {
    match version {
      0 => Self::deserialize_v0(data),
      _ => Err(EngineError::InvalidEntryVersion(version)),
    }
  }

  fn deserialize_v0(data: &[u8]) -> EngineResult<Self> {
    let mut offset = 0;

    let path_length = read_u16(data, &mut offset)? as usize;
    let path = read_string(data, &mut offset, path_length)?;

    let deleted_at = read_i64(data, &mut offset)?;

    let reason_length = read_u16(data, &mut offset)? as usize;
    let reason = if reason_length == 0 { None } else { Some(read_string(data, &mut offset, reason_length)?) };

    Ok(Self { path, deleted_at, reason })
  }
}
