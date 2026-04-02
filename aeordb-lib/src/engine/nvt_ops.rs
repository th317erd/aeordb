use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::nvt::NormalizedVectorTable;

/// A bitmask over NVT buckets. One bit per bucket.
/// Used for compositing query results across multiple indexes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NVTMask {
  bucket_count: usize,
  bits: Vec<u64>,
}

impl NVTMask {
  /// Create a new mask with all bits off.
  pub fn new(bucket_count: usize) -> Self {
    let word_count = Self::words_needed(bucket_count);
    NVTMask {
      bucket_count,
      bits: vec![0u64; word_count],
    }
  }

  /// Create a mask with all bits on (up to bucket_count).
  pub fn all_on(bucket_count: usize) -> Self {
    let word_count = Self::words_needed(bucket_count);
    let mut bits = vec![u64::MAX; word_count];
    // Clear trailing bits beyond bucket_count in the last word.
    let remainder = bucket_count % 64;
    if remainder > 0 && !bits.is_empty() {
      let last_index = bits.len() - 1;
      bits[last_index] = (1u64 << remainder) - 1;
    }
    NVTMask {
      bucket_count,
      bits,
    }
  }

  /// Create a mask from an NVT: bit on if bucket has entries.
  pub fn from_nvt(nvt: &NormalizedVectorTable) -> Self {
    let bucket_count = nvt.bucket_count();
    let mut mask = Self::new(bucket_count);
    for index in 0..bucket_count {
      if nvt.get_bucket(index).entry_count > 0 {
        mask.set_bit(index);
      }
    }
    mask
  }

  /// Create a mask with bits on in the range [start_bucket, end_bucket) (exclusive end).
  pub fn from_range(bucket_count: usize, start_bucket: usize, end_bucket: usize) -> Self {
    let mut mask = Self::new(bucket_count);
    let clamped_start = start_bucket.min(bucket_count);
    let clamped_end = end_bucket.min(bucket_count);
    for index in clamped_start..clamped_end {
      mask.set_bit(index);
    }
    mask
  }

  /// Set a single bit on.
  pub fn set_bit(&mut self, index: usize) {
    if index >= self.bucket_count {
      return;
    }
    let word_index = index / 64;
    let bit_index = index % 64;
    self.bits[word_index] |= 1u64 << bit_index;
  }

  /// Get the value of a single bit.
  pub fn get_bit(&self, index: usize) -> bool {
    if index >= self.bucket_count {
      return false;
    }
    let word_index = index / 64;
    let bit_index = index % 64;
    (self.bits[word_index] & (1u64 << bit_index)) != 0
  }

  /// Clear a single bit.
  pub fn clear_bit(&mut self, index: usize) {
    if index >= self.bucket_count {
      return;
    }
    let word_index = index / 64;
    let bit_index = index % 64;
    self.bits[word_index] &= !(1u64 << bit_index);
  }

  /// Bitwise AND of two masks. Both must have the same bucket_count.
  pub fn and(&self, other: &NVTMask) -> EngineResult<NVTMask> {
    self.require_same_bucket_count(other)?;
    let bits = self.bits.iter()
      .zip(other.bits.iter())
      .map(|(a, b)| a & b)
      .collect();
    Ok(NVTMask {
      bucket_count: self.bucket_count,
      bits,
    })
  }

  /// Bitwise OR of two masks. Both must have the same bucket_count.
  pub fn or(&self, other: &NVTMask) -> EngineResult<NVTMask> {
    self.require_same_bucket_count(other)?;
    let bits = self.bits.iter()
      .zip(other.bits.iter())
      .map(|(a, b)| a | b)
      .collect();
    Ok(NVTMask {
      bucket_count: self.bucket_count,
      bits,
    })
  }

  /// Bitwise NOT of this mask.
  pub fn not(&self) -> NVTMask {
    let mut bits: Vec<u64> = self.bits.iter()
      .map(|word| !word)
      .collect();
    // Clear trailing bits beyond bucket_count in the last word.
    let remainder = self.bucket_count % 64;
    if remainder > 0 && !bits.is_empty() {
      let last_index = bits.len() - 1;
      bits[last_index] &= (1u64 << remainder) - 1;
    }
    NVTMask {
      bucket_count: self.bucket_count,
      bits,
    }
  }

  /// Bitwise XOR of two masks. Both must have the same bucket_count.
  pub fn xor(&self, other: &NVTMask) -> EngineResult<NVTMask> {
    self.require_same_bucket_count(other)?;
    let bits = self.bits.iter()
      .zip(other.bits.iter())
      .map(|(a, b)| a ^ b)
      .collect();
    Ok(NVTMask {
      bucket_count: self.bucket_count,
      bits,
    })
  }

  /// Difference: self AND NOT other. Both must have the same bucket_count.
  pub fn difference(&self, other: &NVTMask) -> EngineResult<NVTMask> {
    self.require_same_bucket_count(other)?;
    let mut bits: Vec<u64> = self.bits.iter()
      .zip(other.bits.iter())
      .map(|(a, b)| a & !b)
      .collect();
    // Clear trailing bits beyond bucket_count in the last word.
    let remainder = self.bucket_count % 64;
    if remainder > 0 && !bits.is_empty() {
      let last_index = bits.len() - 1;
      bits[last_index] &= (1u64 << remainder) - 1;
    }
    Ok(NVTMask {
      bucket_count: self.bucket_count,
      bits,
    })
  }

  /// Count of on bits.
  pub fn popcount(&self) -> usize {
    self.bits.iter()
      .map(|word| word.count_ones() as usize)
      .sum()
  }

  /// Indices of all on bits.
  pub fn surviving_buckets(&self) -> Vec<usize> {
    let mut result = Vec::new();
    for (word_index, &word) in self.bits.iter().enumerate() {
      if word == 0 {
        continue;
      }
      let base = word_index * 64;
      let mut remaining = word;
      while remaining != 0 {
        let trailing_zeros = remaining.trailing_zeros() as usize;
        let bucket_index = base + trailing_zeros;
        if bucket_index < self.bucket_count {
          result.push(bucket_index);
        }
        remaining &= remaining - 1; // clear lowest set bit
      }
    }
    result
  }

  /// Returns true if no bits are on.
  pub fn is_empty(&self) -> bool {
    self.bits.iter().all(|&word| word == 0)
  }

  /// Strided AND: only compare every Nth bucket, propagate result to skipped buckets.
  /// Both must have the same bucket_count.
  pub fn and_strided(&self, other: &NVTMask, stride: usize) -> EngineResult<NVTMask> {
    self.require_same_bucket_count(other)?;
    let stride = stride.max(1);
    let mut result = NVTMask::new(self.bucket_count);
    let mut sampled_index = 0;
    while sampled_index < self.bucket_count {
      let self_on = self.get_bit(sampled_index);
      let other_on = other.get_bit(sampled_index);
      if self_on && other_on {
        // Set all bits in this stride window.
        let window_end = (sampled_index + stride).min(self.bucket_count);
        for bit_index in sampled_index..window_end {
          result.set_bit(bit_index);
        }
      }
      sampled_index += stride;
    }
    Ok(result)
  }

  /// Progressive AND: rough pass at initial_stride, then precise on surviving regions.
  /// Both must have the same bucket_count.
  pub fn and_progressive(&self, other: &NVTMask, initial_stride: usize) -> EngineResult<NVTMask> {
    self.require_same_bucket_count(other)?;
    let initial_stride = initial_stride.max(1);

    // Pass 1: rough scan at stride granularity to find surviving regions.
    let mut surviving_regions: Vec<usize> = Vec::new();
    let mut sampled_index = 0;
    while sampled_index < self.bucket_count {
      if self.get_bit(sampled_index) && other.get_bit(sampled_index) {
        surviving_regions.push(sampled_index);
      }
      sampled_index += initial_stride;
    }

    // Pass 2: precise AND within each surviving region.
    let mut result = NVTMask::new(self.bucket_count);
    for &region_start in &surviving_regions {
      let region_end = (region_start + initial_stride).min(self.bucket_count);
      for bit_index in region_start..region_end {
        if self.get_bit(bit_index) && other.get_bit(bit_index) {
          result.set_bit(bit_index);
        }
      }
    }

    Ok(result)
  }

  /// The number of buckets this mask covers.
  pub fn bucket_count(&self) -> usize {
    self.bucket_count
  }

  // --- Private helpers ---

  fn words_needed(bucket_count: usize) -> usize {
    bucket_count.div_ceil(64)
  }

  fn require_same_bucket_count(&self, other: &NVTMask) -> EngineResult<()> {
    if self.bucket_count != other.bucket_count {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!(
          "NVTMask bucket count mismatch: {} vs {}",
          self.bucket_count, other.bucket_count,
        ),
      });
    }
    Ok(())
  }
}
