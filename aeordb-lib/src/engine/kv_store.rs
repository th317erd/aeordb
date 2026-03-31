use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::nvt::NormalizedVectorTable;
use crate::engine::scalar_converter::HashConverter;

// Lower 4 bits - type
pub const KV_TYPE_CHUNK: u8       = 0x0;
pub const KV_TYPE_FILE_RECORD: u8 = 0x1;
pub const KV_TYPE_DIRECTORY: u8   = 0x2;
pub const KV_TYPE_DELETION: u8    = 0x3;
pub const KV_TYPE_SNAPSHOT: u8    = 0x4;
pub const KV_TYPE_VOID: u8        = 0x5;
pub const KV_TYPE_HEAD: u8        = 0x6;
pub const KV_TYPE_FORK: u8        = 0x7;
pub const KV_TYPE_VERSION: u8     = 0x8;

// Upper 4 bits - flags
pub const KV_FLAG_PENDING: u8     = 0x10;
pub const KV_FLAG_DELETED: u8     = 0x20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KVEntry {
  pub type_flags: u8,
  pub hash: Vec<u8>,
  pub offset: u64,
}

impl KVEntry {
  pub fn entry_type(&self) -> u8 {
    self.type_flags & 0x0F
  }

  pub fn flags(&self) -> u8 {
    self.type_flags & 0xF0
  }

  pub fn is_pending(&self) -> bool {
    self.type_flags & KV_FLAG_PENDING != 0
  }

  pub fn is_deleted(&self) -> bool {
    self.type_flags & KV_FLAG_DELETED != 0
  }
}

#[derive(Debug, Clone)]
pub struct KVStore {
  version: u8,
  hash_algo: HashAlgorithm,
  entries: Vec<KVEntry>,
  nvt: NormalizedVectorTable,
}

impl KVStore {
  pub fn new(hash_algo: HashAlgorithm, initial_nvt_buckets: usize) -> Self {
    KVStore {
      version: 1,
      hash_algo,
      entries: Vec::new(),
      nvt: NormalizedVectorTable::new(Box::new(HashConverter), initial_nvt_buckets),
    }
  }

  pub fn insert(&mut self, entry: KVEntry) {
    // Check for duplicate hash — if found, update in place
    let insertion_point = self.entries.binary_search_by(|existing| existing.hash.cmp(&entry.hash));

    match insertion_point {
      Ok(index) => {
        // Duplicate hash: update existing entry
        self.entries[index] = entry;
      }
      Err(index) => {
        // New entry: insert at sorted position
        self.entries.insert(index, entry);
        self.rebuild_nvt();
      }
    }
  }

  pub fn get(&self, hash: &[u8]) -> Option<&KVEntry> {
    let bucket_index = self.nvt.bucket_for_value(hash);
    let bucket = self.nvt.get_bucket(bucket_index);

    let start = bucket.kv_block_offset as usize;
    let count = bucket.entry_count as usize;

    if start >= self.entries.len() || count == 0 {
      return None;
    }

    let end = (start + count).min(self.entries.len());
    let range = &self.entries[start..end];

    // Binary search within the bucket range
    match range.binary_search_by(|entry| entry.hash.as_slice().cmp(hash)) {
      Ok(relative_index) => Some(&self.entries[start + relative_index]),
      Err(_) => None,
    }
  }

  pub fn remove(&mut self, hash: &[u8]) -> Option<KVEntry> {
    let position = self.entries.binary_search_by(|entry| entry.hash.as_slice().cmp(hash));
    match position {
      Ok(index) => {
        let removed = self.entries.remove(index);
        self.rebuild_nvt();
        Some(removed)
      }
      Err(_) => None,
    }
  }

  pub fn update_offset(&mut self, hash: &[u8], new_offset: u64) -> bool {
    let position = self.entries.binary_search_by(|entry| entry.hash.as_slice().cmp(hash));
    match position {
      Ok(index) => {
        self.entries[index].offset = new_offset;
        true
      }
      Err(_) => false,
    }
  }

  pub fn update_flags(&mut self, hash: &[u8], new_flags: u8) -> bool {
    let position = self.entries.binary_search_by(|entry| entry.hash.as_slice().cmp(hash));
    match position {
      Ok(index) => {
        let entry_type = self.entries[index].type_flags & 0x0F;
        self.entries[index].type_flags = entry_type | (new_flags & 0xF0);
        true
      }
      Err(_) => false,
    }
  }

  pub fn contains(&self, hash: &[u8]) -> bool {
    self.get(hash).is_some()
  }

  pub fn len(&self) -> usize {
    self.entries.len()
  }

  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  pub fn iter(&self) -> impl Iterator<Item = &KVEntry> {
    self.entries.iter()
  }

  pub fn entries_in_range(&self, start_offset: u64, end_offset: u64) -> Vec<&KVEntry> {
    self.entries
      .iter()
      .filter(|entry| entry.offset >= start_offset && entry.offset < end_offset)
      .collect()
  }

  pub fn version(&self) -> u8 {
    self.version
  }

  pub fn hash_algo(&self) -> HashAlgorithm {
    self.hash_algo
  }

  pub fn nvt(&self) -> &NormalizedVectorTable {
    &self.nvt
  }

  pub fn serialize(&self) -> Vec<u8> {
    let hash_length = self.hash_algo.hash_length();
    // version(1) + hash_algo(2) + entry_count(8) + entries + nvt_data
    let entry_size = 1 + hash_length + 8; // type_flags(1) + hash + offset(8)
    let entries_size = self.entries.len() * entry_size;
    let nvt_data = self.nvt.serialize();

    let capacity = 1 + 2 + 8 + entries_size + 4 + nvt_data.len();
    let mut buffer = Vec::with_capacity(capacity);

    buffer.push(self.version);
    buffer.extend_from_slice(&self.hash_algo.to_u16().to_le_bytes());
    buffer.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());

    for entry in &self.entries {
      buffer.push(entry.type_flags);
      buffer.extend_from_slice(&entry.hash);
      buffer.extend_from_slice(&entry.offset.to_le_bytes());
    }

    // Append NVT length then NVT data
    buffer.extend_from_slice(&(nvt_data.len() as u32).to_le_bytes());
    buffer.extend_from_slice(&nvt_data);

    buffer
  }

  pub fn deserialize(data: &[u8]) -> EngineResult<Self> {
    // Minimum: version(1) + hash_algo(2) + entry_count(8) = 11 bytes
    if data.len() < 11 {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "KVStore data too short for header".to_string(),
      });
    }

    let version = data[0];
    if version == 0 {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("Invalid KVStore version: {}", version),
      });
    }

    let hash_algo_raw = u16::from_le_bytes([data[1], data[2]]);
    let hash_algo = HashAlgorithm::from_u16(hash_algo_raw)
      .ok_or(EngineError::InvalidHashAlgorithm(hash_algo_raw))?;

    let entry_count = u64::from_le_bytes([
      data[3], data[4], data[5], data[6], data[7], data[8], data[9], data[10],
    ]) as usize;

    let hash_length = hash_algo.hash_length();
    let entry_size = 1 + hash_length + 8;
    let entries_end = 11 + entry_count * entry_size;

    if data.len() < entries_end + 4 {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!(
          "KVStore data too short: expected at least {} bytes, got {}",
          entries_end + 4,
          data.len()
        ),
      });
    }

    let mut entries = Vec::with_capacity(entry_count);
    let mut cursor = 11;
    for _ in 0..entry_count {
      let type_flags = data[cursor];
      cursor += 1;

      let hash = data[cursor..cursor + hash_length].to_vec();
      cursor += hash_length;

      let offset = u64::from_le_bytes([
        data[cursor],
        data[cursor + 1],
        data[cursor + 2],
        data[cursor + 3],
        data[cursor + 4],
        data[cursor + 5],
        data[cursor + 6],
        data[cursor + 7],
      ]);
      cursor += 8;

      entries.push(KVEntry {
        type_flags,
        hash,
        offset,
      });
    }

    let nvt_length = u32::from_le_bytes([
      data[cursor],
      data[cursor + 1],
      data[cursor + 2],
      data[cursor + 3],
    ]) as usize;
    cursor += 4;

    if data.len() < cursor + nvt_length {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "KVStore data too short for NVT section".to_string(),
      });
    }

    let nvt = NormalizedVectorTable::deserialize(&data[cursor..cursor + nvt_length])?;

    Ok(KVStore {
      version,
      hash_algo,
      entries,
      nvt,
    })
  }

  pub fn rebuild_nvt(&mut self) {
    let bucket_count = self.nvt.bucket_count();

    // Reset all buckets
    for index in 0..bucket_count {
      self.nvt.update_bucket(index, 0, 0);
    }

    if self.entries.is_empty() {
      return;
    }

    // Assign each entry to a bucket and track ranges
    // Since entries are sorted by hash, and hash_to_scalar is monotonic with
    // the first 8 bytes, bucket assignments will be in order.
    let mut current_bucket: Option<usize> = None;
    let mut bucket_start: usize = 0;
    let mut bucket_count_entries: u32 = 0;

    for (entry_index, entry) in self.entries.iter().enumerate() {
      let bucket_index = self.nvt.bucket_for_value(&entry.hash);

      if current_bucket == Some(bucket_index) {
        bucket_count_entries += 1;
      } else {
        // Flush the previous bucket
        if let Some(previous_bucket) = current_bucket {
          self.nvt.update_bucket(previous_bucket, bucket_start as u64, bucket_count_entries);
        }
        current_bucket = Some(bucket_index);
        bucket_start = entry_index;
        bucket_count_entries = 1;
      }
    }

    // Flush the last bucket
    if let Some(previous_bucket) = current_bucket {
      self.nvt.update_bucket(previous_bucket, bucket_start as u64, bucket_count_entries);
    }
  }
}
