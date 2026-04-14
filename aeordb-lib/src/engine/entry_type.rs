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
  Symlink        = 0x08,
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
      0x08 => Ok(EntryType::Symlink),
      _    => Err(EngineError::InvalidEntryType(value)),
    }
  }

  pub fn to_u8(self) -> u8 {
    self as u8
  }

  /// Map this entry type to the corresponding KV store type constant.
  pub fn to_kv_type(self) -> u8 {
    use crate::engine::kv_store::{
      KV_TYPE_CHUNK, KV_TYPE_DELETION, KV_TYPE_DIRECTORY, KV_TYPE_FILE_RECORD,
      KV_TYPE_FORK, KV_TYPE_SNAPSHOT, KV_TYPE_SYMLINK, KV_TYPE_VOID,
    };
    match self {
      EntryType::Chunk => KV_TYPE_CHUNK,
      EntryType::FileRecord => KV_TYPE_FILE_RECORD,
      EntryType::DirectoryIndex => KV_TYPE_DIRECTORY,
      EntryType::DeletionRecord => KV_TYPE_DELETION,
      EntryType::Snapshot => KV_TYPE_SNAPSHOT,
      EntryType::Void => KV_TYPE_VOID,
      EntryType::Fork => KV_TYPE_FORK,
      EntryType::Symlink => KV_TYPE_SYMLINK,
    }
  }
}
