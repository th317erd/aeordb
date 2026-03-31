use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;

#[derive(Debug, Clone)]
pub struct NVTBucket {
  pub kv_block_offset: u64,
  pub entry_count: u32,
}

#[derive(Debug, Clone)]
pub struct NormalizedVectorTable {
  version: u8,
  hash_algo: HashAlgorithm,
  buckets: Vec<NVTBucket>,
}

pub fn hash_to_scalar(hash: &[u8]) -> f64 {
  let bytes: [u8; 8] = hash[..8].try_into().unwrap();
  let value = u64::from_be_bytes(bytes);
  value as f64 / u64::MAX as f64
}

impl NormalizedVectorTable {
  pub fn new(hash_algo: HashAlgorithm, initial_bucket_count: usize) -> Self {
    let buckets = (0..initial_bucket_count)
      .map(|_| NVTBucket {
        kv_block_offset: 0,
        entry_count: 0,
      })
      .collect();

    NormalizedVectorTable {
      version: 1,
      hash_algo,
      buckets,
    }
  }

  pub fn bucket_for_hash(&self, hash: &[u8]) -> usize {
    let scalar = hash_to_scalar(hash);
    let index = (scalar * self.buckets.len() as f64).floor() as usize;
    // Clamp to valid range (scalar of exactly 1.0 would overflow)
    index.min(self.buckets.len().saturating_sub(1))
  }

  pub fn get_bucket(&self, index: usize) -> &NVTBucket {
    &self.buckets[index]
  }

  pub fn update_bucket(&mut self, index: usize, offset: u64, count: u32) {
    self.buckets[index].kv_block_offset = offset;
    self.buckets[index].entry_count = count;
  }

  pub fn resize(&mut self, new_bucket_count: usize) {
    let mut new_buckets: Vec<NVTBucket> = (0..new_bucket_count)
      .map(|_| NVTBucket {
        kv_block_offset: 0,
        entry_count: 0,
      })
      .collect();

    // Redistribute entries from old buckets into new buckets.
    // Each old bucket's entries span a range in the new bucket space.
    let old_count = self.buckets.len();
    for (old_index, old_bucket) in self.buckets.iter().enumerate() {
      if old_bucket.entry_count == 0 {
        continue;
      }

      // The old bucket covers the scalar range [old_index/old_count, (old_index+1)/old_count).
      // Map that to new bucket indices.
      let scalar_start = old_index as f64 / old_count as f64;
      let scalar_end = (old_index + 1) as f64 / old_count as f64;

      let new_start = (scalar_start * new_bucket_count as f64).floor() as usize;
      let new_end = (scalar_end * new_bucket_count as f64).ceil() as usize;
      let new_end = new_end.min(new_bucket_count);

      let span = new_end - new_start;
      if span == 0 {
        continue;
      }

      // Distribute entries evenly across the new buckets in the range.
      // The first bucket gets the offset; entries are split evenly.
      let entries_per_bucket = old_bucket.entry_count / span as u32;
      let remainder = old_bucket.entry_count % span as u32;

      let mut current_offset = old_bucket.kv_block_offset;
      for (position, new_bucket) in new_buckets[new_start..new_end].iter_mut().enumerate() {
        let extra = if (position as u32) < remainder { 1 } else { 0 };
        let count = entries_per_bucket + extra;
        new_bucket.kv_block_offset = current_offset;
        new_bucket.entry_count = count;
        current_offset += count as u64;
      }
    }

    self.buckets = new_buckets;
  }

  pub fn bucket_count(&self) -> usize {
    self.buckets.len()
  }

  pub fn version(&self) -> u8 {
    self.version
  }

  pub fn hash_algo(&self) -> HashAlgorithm {
    self.hash_algo
  }

  pub fn serialize(&self) -> Vec<u8> {
    // version(1) + hash_algo(2) + bucket_count(4) + buckets(12 each)
    let capacity = 1 + 2 + 4 + self.buckets.len() * 12;
    let mut buffer = Vec::with_capacity(capacity);

    buffer.push(self.version);
    buffer.extend_from_slice(&self.hash_algo.to_u16().to_le_bytes());
    buffer.extend_from_slice(&(self.buckets.len() as u32).to_le_bytes());

    for bucket in &self.buckets {
      buffer.extend_from_slice(&bucket.kv_block_offset.to_le_bytes());
      buffer.extend_from_slice(&bucket.entry_count.to_le_bytes());
    }

    buffer
  }

  pub fn deserialize(data: &[u8]) -> EngineResult<Self> {
    // Minimum: version(1) + hash_algo(2) + bucket_count(4) = 7 bytes
    if data.len() < 7 {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "NVT data too short for header".to_string(),
      });
    }

    let version = data[0];
    if version == 0 {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("Invalid NVT version: {}", version),
      });
    }

    let hash_algo_raw = u16::from_le_bytes([data[1], data[2]]);
    let hash_algo = HashAlgorithm::from_u16(hash_algo_raw)
      .ok_or(EngineError::InvalidHashAlgorithm(hash_algo_raw))?;

    let bucket_count = u32::from_le_bytes([data[3], data[4], data[5], data[6]]) as usize;

    let expected_length = 7 + bucket_count * 12;
    if data.len() < expected_length {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!(
          "NVT data too short: expected {} bytes for {} buckets, got {}",
          expected_length, bucket_count, data.len()
        ),
      });
    }

    let mut buckets = Vec::with_capacity(bucket_count);
    let mut cursor = 7;
    for _ in 0..bucket_count {
      let kv_block_offset = u64::from_le_bytes([
        data[cursor],
        data[cursor + 1],
        data[cursor + 2],
        data[cursor + 3],
        data[cursor + 4],
        data[cursor + 5],
        data[cursor + 6],
        data[cursor + 7],
      ]);
      let entry_count = u32::from_le_bytes([
        data[cursor + 8],
        data[cursor + 9],
        data[cursor + 10],
        data[cursor + 11],
      ]);
      buckets.push(NVTBucket {
        kv_block_offset,
        entry_count,
      });
      cursor += 12;
    }

    Ok(NormalizedVectorTable {
      version,
      hash_algo,
      buckets,
    })
  }
}
