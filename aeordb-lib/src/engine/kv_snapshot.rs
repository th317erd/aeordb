use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use crate::engine::errors::EngineResult;
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::kv_pages::{deserialize_page, find_entry_in_page_data, live_type_counts_in_page};
use crate::engine::kv_store::{KVEntry, KV_FLAG_DELETED};
use crate::engine::nvt::NormalizedVectorTable;

pub type KvPageSet = Arc<Vec<Arc<[u8]>>>;
pub type KvTypeCounts = [usize; 16];

/// An immutable, lock-free read view of the KV store.
///
/// Holds a frozen snapshot of the write buffer, shared NVT state, and an
/// in-memory copy of all KV pages at snapshot creation time. Each snapshot is
/// fully self-contained: buffer + NVT + pages. Reads are served entirely from
/// memory -- no disk I/O, no race conditions with concurrent writers.
///
/// NOTE: At max KV stage (131K buckets x 1.3KB/page = ~164MB), the pages
/// Vec uses significant memory. Old snapshots survive via Arc until all
/// readers drop their references. Under sustained read load, multiple
/// snapshot generations can coexist. Monitor memory usage at scale.
pub struct ReadSnapshot {
  /// Frozen copy of the write buffer at snapshot creation time.
  buffer: HashMap<Vec<u8>, KVEntry>,
  /// Shared NVT for O(1) bucket lookup.
  nvt: Arc<NormalizedVectorTable>,
  /// Number of NVT buckets (cached from nvt for convenience).
  bucket_count: usize,
  /// Hash algorithm (determines hash_length for page layout).
  hash_algo: HashAlgorithm,
  /// Total entry count at snapshot creation time.
  entry_count: usize,
  /// In-memory copy of all KV pages at snapshot time. Each bucket page is
  /// individually Arc-wrapped, so publishing a new snapshot only clones page
  /// pointers and replaces the buckets that actually changed.
  pages: KvPageSet,
  /// Live entry counts for flushed KV pages only. Buffer overrides are folded
  /// in by `count_by_type`; explicit type iteration scans pages on demand.
  page_type_counts: KvTypeCounts,
  /// Type counts for the current write buffer.
  buffer_type_counts: KvTypeCounts,
}

impl fmt::Debug for ReadSnapshot {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("ReadSnapshot")
      .field("bucket_count", &self.bucket_count)
      .field("hash_algo", &self.hash_algo)
      .field("entry_count", &self.entry_count)
      .field("buffer_len", &self.buffer.len())
      .field("pages", &format_args!("Arc<Vec<{} pages>>", self.pages.len()))
      .finish_non_exhaustive()
  }
}

impl ReadSnapshot {
  /// Create a new read snapshot from a frozen buffer, shared NVT, and shared
  /// immutable KV pages.
  pub fn new(
    buffer: HashMap<Vec<u8>, KVEntry>,
    nvt: Arc<NormalizedVectorTable>,
    bucket_count: usize,
    hash_algo: HashAlgorithm,
    entry_count: usize,
    pages: KvPageSet,
  ) -> Self {
    let page_type_counts = Self::build_page_type_counts(&pages, hash_algo);
    let buffer_type_counts = Self::build_buffer_type_counts(&buffer);
    ReadSnapshot { buffer, nvt, bucket_count, hash_algo, entry_count, pages, page_type_counts, buffer_type_counts }
  }

  /// Create a new snapshot while reusing precomputed flushed-page counts.
  /// Use this when only the write buffer changed or when the publisher has
  /// already applied bucket-local count deltas.
  pub fn new_with_page_type_counts(
    buffer: HashMap<Vec<u8>, KVEntry>,
    nvt: Arc<NormalizedVectorTable>,
    bucket_count: usize,
    hash_algo: HashAlgorithm,
    entry_count: usize,
    pages: KvPageSet,
    page_type_counts: KvTypeCounts,
  ) -> Self {
    let buffer_type_counts = Self::build_buffer_type_counts(&buffer);
    ReadSnapshot { buffer, nvt, bucket_count, hash_algo, entry_count, pages, page_type_counts, buffer_type_counts }
  }

  /// Build compact type counts from flushed KV pages only. Deleted entries are
  /// excluded. Corrupt pages are treated as empty here; open/flush paths are
  /// responsible for flagging corruption and triggering rebuild.
  fn build_page_type_counts(pages: &[Arc<[u8]>], hash_algo: HashAlgorithm) -> KvTypeCounts {
    let hash_length = hash_algo.hash_length();
    let mut counts = [0usize; 16];
    for page_data in pages.iter() {
      if let Ok(page_counts) = live_type_counts_in_page(page_data, hash_length) {
        for (i, count) in page_counts.iter().enumerate() {
          counts[i] += count;
        }
      }
    }
    counts
  }

  /// Build compact type counts from the write buffer only.
  fn build_buffer_type_counts(buffer: &HashMap<Vec<u8>, KVEntry>) -> KvTypeCounts {
    let mut counts = [0usize; 16];
    for entry in buffer.values() {
      if (entry.type_flags & KV_FLAG_DELETED) != 0 {
        continue;
      }
      counts[entry.entry_type() as usize] += 1;
    }
    counts
  }

  /// Access the shared pages Arc (for cheap cloning in buffer-only publishes).
  pub fn pages(&self) -> &KvPageSet {
    &self.pages
  }

  /// Access flushed-page type counts for reuse when publishing a new snapshot.
  pub fn page_type_counts(&self) -> KvTypeCounts {
    self.page_type_counts
  }

  /// Look up an entry by hash. Checks the buffer first, then reads
  /// from disk via a cloned file handle. Returns `None` for deleted entries.
  pub fn get(&self, hash: &[u8]) -> Option<KVEntry> {
    // 1. Check buffer first (most recent writes at snapshot time)
    if let Some(entry) = self.buffer.get(hash) {
      if entry.is_deleted() {
        return None;
      }
      return Some(entry.clone());
    }

    // 2. Read from disk via NVT bucket mapping
    self.read_from_disk(hash, false)
  }

  /// Same as `get` but returns deleted entries too (needed for `is_entry_deleted` checks).
  pub fn get_raw(&self, hash: &[u8]) -> Option<KVEntry> {
    // 1. Check buffer first
    if let Some(entry) = self.buffer.get(hash) {
      return Some(entry.clone());
    }

    // 2. Read from disk — include deleted entries
    self.read_from_disk(hash, true)
  }

  /// Read a single entry from the in-memory pages by hash.
  /// When `include_deleted` is true, returns entries even if they have the deleted flag.
  fn read_from_disk(&self, hash: &[u8], include_deleted: bool) -> Option<KVEntry> {
    let bucket_index = self.nvt.bucket_for_value(hash);
    if bucket_index >= self.bucket_count || bucket_index >= self.pages.len() {
      return None;
    }

    let hash_length = self.hash_algo.hash_length();
    let page_data = &self.pages[bucket_index];

    find_entry_in_page_data(page_data, hash_length, hash, include_deleted).ok().flatten()
  }

  /// Iterate all entries of a specific type. This intentionally scans pages on
  /// demand instead of keeping a full always-resident cloned type index.
  pub fn iter_by_type(&self, target_type: u8) -> Vec<KVEntry> {
    let mut entries = Vec::new();
    let hash_length = self.hash_algo.hash_length();

    for page_data in self.pages.iter() {
      if let Ok(page_entries) = deserialize_page(page_data, hash_length) {
        for entry in page_entries {
          if entry.entry_type() == target_type && !entry.is_deleted() && !self.buffer.contains_key(entry.hash.as_slice()) {
            entries.push(entry);
          }
        }
      }
    }

    for entry in self.buffer.values() {
      if entry.entry_type() == target_type && !entry.is_deleted() {
        entries.push(entry.clone());
      }
    }
    entries
  }

  /// Count entries of a specific type without cloning entries.
  pub fn count_by_type(&self, target_type: u8) -> usize {
    let target_index = (target_type & 0x0F) as usize;
    let mut count = self.page_type_counts[target_index];

    for (hash, buffered) in &self.buffer {
      if let Some(page_entry) = self.read_from_disk(hash, false) {
        if page_entry.entry_type() == target_type {
          count = count.saturating_sub(1);
        }
      }
      if !buffered.is_deleted() && buffered.entry_type() == target_type {
        count += 1;
      }
    }

    // Fast path for empty buffers avoids the loop above; this also keeps the
    // precomputed buffer count used so accidental divergence is visible in
    // debug builds during tests.
    debug_assert_eq!(
      self.buffer.values().filter(|entry| !entry.is_deleted() && entry.entry_type() == target_type).count(),
      self.buffer_type_counts[target_index]
    );

    count
  }

  /// Iterate all entries. Explicit O(n) scan used by diagnostics, backup, and
  /// resize paths; normal reads/counts should not call this.
  pub fn iter_all(&self) -> EngineResult<Vec<KVEntry>> {
    let mut all = Vec::new();
    let hash_length = self.hash_algo.hash_length();

    for page_data in self.pages.iter() {
      if let Ok(page_entries) = deserialize_page(page_data, hash_length) {
        for entry in page_entries {
          if !entry.is_deleted() && !self.buffer.contains_key(entry.hash.as_slice()) {
            all.push(entry);
          }
        }
      }
    }

    for entry in self.buffer.values() {
      if !entry.is_deleted() {
        all.push(entry.clone());
      }
    }
    Ok(all)
  }

  /// Check if an entry is marked as deleted in the buffer.
  pub fn is_deleted_in_buffer(&self, hash: &[u8]) -> bool {
    self.buffer.get(hash).map(|e| (e.type_flags & KV_FLAG_DELETED) != 0).unwrap_or(false)
  }

  /// Total entry count at snapshot creation time.
  pub fn len(&self) -> usize {
    self.entry_count
  }

  /// Whether the snapshot has zero entries.
  pub fn is_empty(&self) -> bool {
    self.entry_count == 0
  }

  /// Number of NVT buckets.
  pub fn bucket_count(&self) -> usize {
    self.bucket_count
  }

  /// Hash algorithm used by this snapshot.
  pub fn hash_algo(&self) -> HashAlgorithm {
    self.hash_algo
  }

  /// Number of entries in the frozen buffer.
  pub fn buffer_len(&self) -> usize {
    self.buffer.len()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::engine::hash_algorithm::HashAlgorithm;
  use crate::engine::kv_pages::serialize_page;
  use crate::engine::kv_store::{KV_TYPE_CHUNK, KV_TYPE_DIRECTORY, KV_TYPE_FILE_RECORD};
  use crate::engine::scalar_converter::HashConverter;

  fn entry(hash_byte: u8, entry_type: u8, deleted: bool) -> KVEntry {
    KVEntry {
      type_flags: entry_type | if deleted { KV_FLAG_DELETED } else { 0 },
      hash: vec![hash_byte; 32],
      offset: hash_byte as u64 * 100,
      total_length: 128,
    }
  }

  fn snapshot_with(buffer: HashMap<Vec<u8>, KVEntry>) -> ReadSnapshot {
    let page = serialize_page(&[entry(1, KV_TYPE_CHUNK, false), entry(2, KV_TYPE_FILE_RECORD, false)], 32);
    let pages = Arc::new(vec![Arc::<[u8]>::from(page.into_boxed_slice())]);
    let nvt = Arc::new(NormalizedVectorTable::new(Box::new(HashConverter), 1));
    ReadSnapshot::new(buffer, nvt, 1, HashAlgorithm::Blake3_256, 2, pages)
  }

  #[test]
  fn count_by_type_applies_buffer_overrides() {
    let mut buffer = HashMap::new();
    buffer.insert(vec![2u8; 32], entry(2, KV_TYPE_DIRECTORY, false));
    let snapshot = snapshot_with(buffer);

    assert_eq!(snapshot.count_by_type(KV_TYPE_CHUNK), 1);
    assert_eq!(snapshot.count_by_type(KV_TYPE_FILE_RECORD), 0);
    assert_eq!(snapshot.count_by_type(KV_TYPE_DIRECTORY), 1);
  }

  #[test]
  fn count_by_type_applies_buffer_deletions() {
    let mut buffer = HashMap::new();
    buffer.insert(vec![1u8; 32], entry(1, KV_TYPE_CHUNK, true));
    let snapshot = snapshot_with(buffer);

    assert_eq!(snapshot.count_by_type(KV_TYPE_CHUNK), 0);
    assert_eq!(snapshot.count_by_type(KV_TYPE_FILE_RECORD), 1);
  }
}
