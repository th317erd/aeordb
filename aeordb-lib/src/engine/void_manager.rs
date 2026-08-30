use std::collections::{BTreeMap, BTreeSet};

use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::entry_header::EntryHeader;

/// Smallest void worth tracking at all. Voids strictly smaller than this are
/// dropped — they're indistinguishable from byte-alignment noise and would
/// bloat the void index without ever being useful. Default is 1 (track any
/// non-zero gap); set higher if you want to suppress micro-gaps in metrics.
pub const MINIMUM_VOID_SIZE: u32 = 1;

/// Smallest void that can still hold a real entry (header + zero-length key+value).
/// Voids smaller than this are still TRACKED (counted in metrics, visible to
/// fragmentation analysis) but never returned by `find_void` because no entry
/// would fit. For BLAKE3-256: fixed_header(31) + hash(32) + key(0) + value(0) = 63.
pub const MINIMUM_USEFUL_VOID_SIZE: u32 = 63;

/// Tracks reusable free space (voids) in the data file.
///
/// Two parallel indexes keep operations cheap regardless of access pattern:
///   * `by_offset` — `offset → size`. O(log N) dedup on register, ordered
///     iteration (used for gap-scan reconciliation and metrics).
///   * `by_size` — `size → set of offsets`. O(log N) best-fit lookup for
///     writers asking "give me a void of at least N bytes."
///
/// Voids of every size are tracked, including those too small to hold an
/// entry. The engine's allocator filters by `MINIMUM_USEFUL_VOID_SIZE` at
/// query time; tracking everything lets us report fragmentation accurately
/// in metrics and reason about coalescing later.
///
/// VoidManager state is purely in-memory. It is rebuilt at startup from the
/// hot tail (clean) or by gap-scanning the rebuilt KV (dirty).
#[derive(Debug)]
pub struct VoidManager {
  by_offset: BTreeMap<u64, u32>,
  by_size: BTreeMap<u32, BTreeSet<u64>>,
  hash_algo: HashAlgorithm,
}

impl VoidManager {
  pub fn new(hash_algo: HashAlgorithm) -> Self {
    VoidManager { by_offset: BTreeMap::new(), by_size: BTreeMap::new(), hash_algo }
  }

  /// Compute the deterministic hash for a void of the given size.
  /// Retained for backward-compatibility with any callers expecting the
  /// "::aeordb:void:{size}" key convention from the original on-disk format.
  pub fn void_hash(size: u32) -> Vec<u8> {
    let input = format!("::aeordb:void:{}", size);
    let hash = blake3::hash(input.as_bytes());
    hash.as_bytes().to_vec()
  }

  /// Register a void at the given offset with the given size.
  ///
  /// **Floor:** voids strictly smaller than [`MINIMUM_VOID_SIZE`] are silently
  /// dropped — they're alignment noise, not real reclaimable space. Voids
  /// between MINIMUM_VOID_SIZE and MINIMUM_USEFUL_VOID_SIZE are tracked
  /// (for metrics + fragmentation analysis) but won't be returned by
  /// `find_void` because no entry would fit.
  ///
  /// **Normalizes the reusable-space union.** Overlapping and adjacent ranges
  /// are coalesced before they reach either index, so recovery and a later GC
  /// pass cannot publish the same bytes more than once. Unions larger than the
  /// on-disk `u32` length field are retained as adjacent non-overlapping rows.
  pub fn register_void(&mut self, offset: u64, size: u32) {
    if size < MINIMUM_VOID_SIZE {
      return;
    }

    let Some(mut union_end) = offset.checked_add(size as u64) else {
      // Keep impossible legacy evidence visible to validation and metrics;
      // the allocator already refuses to consume an overflowing extent.
      self.insert_exact(offset, size);
      return;
    };
    let mut union_start = offset;

    loop {
      let predecessor = self.by_offset.range(..=union_start).next_back().and_then(|(&existing_offset, &existing_size)| {
        let existing_end = existing_offset.checked_add(existing_size as u64)?;
        (existing_end >= union_start).then_some((existing_offset, existing_size, existing_end))
      });
      let overlap = match predecessor {
        Some(predecessor) => Some(predecessor),
        None => self.by_offset.range(union_start..=union_end).next().and_then(|(&existing_offset, &existing_size)| {
          let existing_end = existing_offset.checked_add(existing_size as u64)?;
          (existing_offset <= union_end).then_some((existing_offset, existing_size, existing_end))
        }),
      };
      let Some((existing_offset, _existing_size, existing_end)) = overlap else {
        break;
      };
      self.remove_void(existing_offset);
      union_start = union_start.min(existing_offset);
      union_end = union_end.max(existing_end);
    }

    let mut cursor = union_start;
    let mut remaining = union_end.saturating_sub(union_start);
    while remaining > 0 {
      let chunk = remaining.min(u32::MAX as u64) as u32;
      self.insert_exact(cursor, chunk);
      cursor = cursor.saturating_add(chunk as u64);
      remaining -= chunk as u64;
    }
  }

  fn insert_exact(&mut self, offset: u64, size: u32) {
    if let Some(existing_size) = self.by_offset.insert(offset, size) {
      if let Some(set) = self.by_size.get_mut(&existing_size) {
        set.remove(&offset);
        if set.is_empty() {
          self.by_size.remove(&existing_size);
        }
      }
    }
    self.by_size.entry(size).or_default().insert(offset);
  }

  /// Find the smallest void that can fit `needed_size` bytes without
  /// stranding an unencodable remainder, remove it from the manager, and
  /// return `(offset, actual_size)`. A larger match is split only when its
  /// remainder can hold a physical Void entry; poor fits remain available
  /// for a later exact or smaller allocation.
  pub fn find_void(&mut self, needed_size: u32) -> Option<(u64, u32)> {
    let minimum_remainder = self.minimum_useful_void_size();
    let (matching_size, offset) = self.by_size.range(needed_size..).find_map(|(&size, offsets)| {
      let remainder = size - needed_size;
      if remainder != 0 && remainder < minimum_remainder {
        return None;
      }
      offsets.iter().copied().find(|offset| remainder == 0 || offset.checked_add(needed_size as u64).is_some()).map(|offset| (size, offset))
    })?;

    let set = self.by_size.get_mut(&matching_size)?;
    set.remove(&offset);
    if set.is_empty() {
      self.by_size.remove(&matching_size);
    }
    self.by_offset.remove(&offset);

    // Split remainder into a smaller void if it can hold a real entry.
    let remainder = matching_size - needed_size;
    if remainder != 0 {
      let remainder_offset = offset.checked_add(needed_size as u64)?;
      self.register_void(remainder_offset, remainder);
    }

    Some((offset, matching_size))
  }

  /// Remove a specific void by offset. Returns the size that was removed
  /// (if any) for callers that want to confirm what was removed.
  pub fn remove_void(&mut self, offset: u64) -> Option<u32> {
    let size = self.by_offset.remove(&offset)?;
    if let Some(set) = self.by_size.get_mut(&size) {
      set.remove(&offset);
      if set.is_empty() {
        self.by_size.remove(&size);
      }
    }
    Some(size)
  }

  /// Total bytes of free space across all tracked voids (all sizes).
  pub fn total_void_space(&self) -> u64 {
    self.by_offset.values().map(|&size| size as u64).sum()
  }

  /// Total number of tracked voids.
  pub fn void_count(&self) -> usize {
    self.by_offset.len()
  }

  /// Estimate the resident bytes owned by both in-memory lookup indexes.
  pub fn estimated_memory_bytes(&self) -> u64 {
    let offset_rows =
      self.by_offset.len().saturating_mul(std::mem::size_of::<(u64, u32)>().saturating_add(4 * std::mem::size_of::<usize>()));
    let size_rows = self.by_size.values().fold(0usize, |total, offsets| {
      total.saturating_add(
        std::mem::size_of::<u32>()
          .saturating_add(4 * std::mem::size_of::<usize>())
          .saturating_add(offsets.len().saturating_mul(std::mem::size_of::<u64>().saturating_add(4 * std::mem::size_of::<usize>()))),
      )
    });
    std::mem::size_of::<Self>().saturating_add(offset_rows).saturating_add(size_rows) as u64
  }

  /// Iterate voids in offset order: `(offset, size)`. Used by the hot tail
  /// flush and by metrics reporting.
  pub fn iter(&self) -> impl Iterator<Item = (u64, u32)> + '_ {
    self.by_offset.iter().map(|(&o, &s)| (o, s))
  }

  /// Return whether a half-open byte range overlaps the current reusable
  /// space map. Registered voids are maintained as non-overlapping extents,
  /// so only the predecessor of the requested end can intersect it.
  pub fn overlaps_range(&self, offset: u64, size: u32) -> bool {
    if size == 0 {
      return false;
    }
    let Some(end) = offset.checked_add(size as u64) else {
      return true;
    };
    self.by_offset.range(..end).next_back().is_some_and(|(&void_offset, &void_size)| void_offset.saturating_add(void_size as u64) > offset)
  }

  /// Smallest entry that could be written into a void of this size: the
  /// fixed entry header + the hash, with zero-byte key+value. Below this
  /// size, a void cannot hold any real entry — but it's still tracked.
  pub fn minimum_useful_void_size(&self) -> u32 {
    EntryHeader::compute_total_length(self.hash_algo, 0, 0).expect("min void size with zero lengths cannot fail bounds")
  }

  /// Replace the current void set with the supplied (offset, size) pairs.
  /// Used by startup recovery (hot tail load, gap-scan) to bulk-populate.
  pub fn replace_all(&mut self, voids: impl IntoIterator<Item = (u64, u32)>) {
    self.by_offset.clear();
    self.by_size.clear();
    for (offset, size) in voids {
      self.register_void(offset, size);
    }
  }
}
