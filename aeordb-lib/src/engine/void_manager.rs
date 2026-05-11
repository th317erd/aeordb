use std::collections::BTreeMap;

use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::entry_header::EntryHeader;

/// Minimum useful void size in bytes.
///
/// A void must be large enough to hold at least the smallest possible entry:
/// fixed_header(31) + hash(N) + key(0) + value(0).
/// For BLAKE3_256 (32-byte hash): 31 + 32 = 63 bytes.
///
/// We use 63 as the default since BLAKE3_256 is the current default algorithm.
/// Any remainder smaller than this after splitting a void is abandoned (not tracked).
pub const MINIMUM_VOID_SIZE: u32 = 63;

/// Tracks reusable free space (voids) in the data file.
///
/// Voids are created when entries are relocated (e.g., during KV block growth)
/// or when entries are logically deleted. The VoidManager maintains an in-memory
/// index of void locations organized by size for best-fit allocation.
///
/// NOTE: The voids_by_size BTreeMap grows without bound as new void sizes
/// are registered. With diverse entry sizes, this can accumulate many unique
/// keys. Consider bucketing void sizes into size classes (powers of 2) or
/// adding a maximum tracked void count with eviction.
pub struct VoidManager {
  /// Maps void size to a list of file offsets where voids of that size exist.
  voids_by_size: BTreeMap<u32, Vec<u64>>,
  hash_algo: HashAlgorithm,
}

impl VoidManager {
  pub fn new(hash_algo: HashAlgorithm) -> Self {
    VoidManager {
      voids_by_size: BTreeMap::new(),
      hash_algo,
    }
  }

  /// Compute the deterministic hash for a void of the given size.
  /// Uses the domain-prefixed format: BLAKE3("::aeordb:void:{size}")
  pub fn void_hash(size: u32) -> Vec<u8> {
    let input = format!("::aeordb:void:{}", size);
    let hash = blake3::hash(input.as_bytes());
    hash.as_bytes().to_vec()
  }

  /// Register a void at the given offset with the given size.
  pub fn register_void(&mut self, size: u32, offset: u64) {
    if size < self.minimum_void_size() {
      return;
    }
    self.voids_by_size
      .entry(size)
      .or_default()
      .push(offset);
  }

  /// Find the smallest void that can fit `needed_size` bytes.
  ///
  /// Returns `Some((offset, actual_size))` if a suitable void is found.
  /// The void is removed from tracking. If the void is larger than needed
  /// and the remainder is >= minimum void size, the remainder is registered
  /// as a new, smaller void.
  pub fn find_void(&mut self, needed_size: u32) -> Option<(u64, u32)> {
    // Find the smallest size >= needed_size using BTreeMap range
    let matching_size = self.voids_by_size
      .range(needed_size..)
      .find(|(_, offsets)| !offsets.is_empty())
      .map(|(&size, _)| size)?;

    let offsets = self.voids_by_size.get_mut(&matching_size)?;
    let offset = offsets.pop()?;

    // Clean up empty size buckets
    if offsets.is_empty() {
      self.voids_by_size.remove(&matching_size);
    }

    // If the void is larger than needed, split the remainder
    let remainder = matching_size - needed_size;
    let min_size = self.minimum_void_size();
    if remainder >= min_size {
      let remainder_offset = offset + needed_size as u64;
      self.register_void(remainder, remainder_offset);
    }

    Some((offset, matching_size))
  }

  /// Remove a specific void by size and offset.
  pub fn remove_void(&mut self, size: u32, offset: u64) {
    if let Some(offsets) = self.voids_by_size.get_mut(&size) {
      if let Some(position) = offsets.iter().position(|&stored_offset| stored_offset == offset) {
        offsets.remove(position);
      }
      if offsets.is_empty() {
        self.voids_by_size.remove(&size);
      }
    }
  }

  /// Total bytes of free space across all tracked voids.
  pub fn total_void_space(&self) -> u64 {
    self.voids_by_size
      .iter()
      .map(|(&size, offsets)| size as u64 * offsets.len() as u64)
      .sum()
  }

  /// Total number of tracked voids.
  pub fn void_count(&self) -> usize {
    self.voids_by_size
      .values()
      .map(|offsets| offsets.len())
      .sum()
  }

  /// The minimum void size for the configured hash algorithm.
  /// This is the smallest possible entry: fixed_header + hash + 0 key + 0 value.
  pub fn minimum_void_size(&self) -> u32 {
    // 0,0 lengths can never fail the bounds check, so unwrap is safe.
    EntryHeader::compute_total_length(self.hash_algo, 0, 0)
      .expect("min void size with zero lengths cannot fail bounds")
  }
}
