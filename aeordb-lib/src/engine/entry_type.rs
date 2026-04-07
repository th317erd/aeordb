use crate::engine::errors::EngineError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EntryType {
  Chunk          = 0x01,
  FileRecord     = 0x02,
  DirectoryIndex = 0x03,
  DeletionRecord = 0x04,
  Snapshot       = 0x05,
  Void           = 0x06,
  Fork           = 0x07,
}

impl EntryType {
  pub fn from_u8(value: u8) -> Result<Self, EngineError> {
    match value {
      0x01 => Ok(EntryType::Chunk),
      0x02 => Ok(EntryType::FileRecord),
      0x03 => Ok(EntryType::DirectoryIndex),
      0x04 => Ok(EntryType::DeletionRecord),
      0x05 => Ok(EntryType::Snapshot),
      0x06 => Ok(EntryType::Void),
      0x07 => Ok(EntryType::Fork),
      _    => Err(EngineError::InvalidEntryType(value)),
    }
  }

  pub fn to_u8(self) -> u8 {
    self as u8
  }
}
