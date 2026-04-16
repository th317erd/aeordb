use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;

/// A symlink entry — stores a target path that this symlink points to.
#[derive(Debug, Clone, PartialEq)]
pub struct SymlinkRecord {
    pub path: String,
    pub target: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl SymlinkRecord {
    pub fn new(path: String, target: String) -> Self {
        let now = chrono::Utc::now().timestamp_millis();
        SymlinkRecord {
            path,
            target,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn serialize(&self) -> Vec<u8> {
        let path_bytes = self.path.as_bytes();
        let target_bytes = self.target.as_bytes();
        let capacity = 2 + path_bytes.len() + 2 + target_bytes.len() + 8 + 8;
        let mut buffer = Vec::with_capacity(capacity);

        buffer.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
        buffer.extend_from_slice(path_bytes);
        buffer.extend_from_slice(&(target_bytes.len() as u16).to_le_bytes());
        buffer.extend_from_slice(target_bytes);
        buffer.extend_from_slice(&self.created_at.to_le_bytes());
        buffer.extend_from_slice(&self.updated_at.to_le_bytes());

        buffer
    }

    pub fn deserialize(data: &[u8], version: u8) -> EngineResult<Self> {
        match version {
            0 => Self::deserialize_v0(data),
            _ => Self::deserialize_v0(data), // future versions will have their own methods
        }
    }

    fn deserialize_v0(data: &[u8]) -> EngineResult<Self> {
        if data.len() < 4 {
            return Err(EngineError::CorruptEntry {
                offset: 0,
                reason: "SymlinkRecord data too short".to_string(),
            });
        }

        let mut cursor = 0;

        let path_length = u16::from_le_bytes([data[cursor], data[cursor + 1]]) as usize;
        cursor += 2;

        if data.len() < cursor + path_length + 2 {
            return Err(EngineError::CorruptEntry {
                offset: 0,
                reason: "SymlinkRecord data too short for path".to_string(),
            });
        }

        let path = String::from_utf8_lossy(&data[cursor..cursor + path_length]).to_string();
        cursor += path_length;

        let target_length = u16::from_le_bytes([data[cursor], data[cursor + 1]]) as usize;
        cursor += 2;

        if data.len() < cursor + target_length + 16 {
            return Err(EngineError::CorruptEntry {
                offset: 0,
                reason: "SymlinkRecord data too short for target + timestamps".to_string(),
            });
        }

        let target = String::from_utf8_lossy(&data[cursor..cursor + target_length]).to_string();
        cursor += target_length;

        let created_at = i64::from_le_bytes(data[cursor..cursor + 8].try_into().unwrap());
        cursor += 8;

        let updated_at = i64::from_le_bytes(data[cursor..cursor + 8].try_into().unwrap());

        Ok(SymlinkRecord {
            path,
            target,
            created_at,
            updated_at,
        })
    }
}

/// Compute the domain-prefixed hash for a symlink path (mutable key for reads/deletion).
pub fn symlink_path_hash(path: &str, algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
    algo.compute_hash(format!("symlink:{}", path).as_bytes())
}

/// Compute the content-addressed hash for a serialized SymlinkRecord (immutable key for versioning).
pub fn symlink_content_hash(data: &[u8], algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
    let mut input = Vec::with_capacity(9 + data.len());
    input.extend_from_slice(b"symlinkc:");
    input.extend_from_slice(data);
    algo.compute_hash(&input)
}
