use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use crate::engine::errors::EngineResult;
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::kv_pages::{deserialize_page, find_in_page};
use crate::engine::kv_store::{KVEntry, KV_FLAG_DELETED};
use crate::engine::nvt::NormalizedVectorTable;

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
    /// In-memory copy of all KV pages at snapshot time.
    /// Each entry is a serialized page (page_size bytes).
    /// Arc-wrapped for cheap sharing between snapshots (buffer-only publishes
    /// reuse existing pages via Arc::clone instead of re-reading from disk).
    pages: Arc<Vec<Vec<u8>>>,
    /// Type index: maps entry_type (lower 4 bits of type_flags) to the set of
    /// hash keys for that type. Built once from pages + buffer at snapshot
    /// creation time; lookups are O(1) by type + O(k) by entries of that type.
    /// Arc-wrapped so buffer-only publishes can share the base page index and
    /// only apply buffer deltas.
    type_index: Arc<HashMap<u8, HashMap<Vec<u8>, KVEntry>>>,
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
    /// Create a new read snapshot from a frozen buffer, shared NVT, and an
    /// in-memory copy of all KV pages. Builds a type index from pages + buffer
    /// so that `iter_by_type()` is O(k) instead of O(n).
    pub fn new(
        buffer: HashMap<Vec<u8>, KVEntry>,
        nvt: Arc<NormalizedVectorTable>,
        bucket_count: usize,
        hash_algo: HashAlgorithm,
        entry_count: usize,
        pages: Arc<Vec<Vec<u8>>>,
    ) -> Self {
        let type_index = Arc::new(Self::build_type_index(&pages, &buffer, hash_algo));
        ReadSnapshot {
            buffer,
            nvt,
            bucket_count,
            hash_algo,
            entry_count,
            pages,
            type_index,
        }
    }

    /// Build the type index from pages + buffer. Entries are grouped by their
    /// entry_type (lower 4 bits). Buffer entries override page entries for the
    /// same hash. Deleted entries are excluded.
    fn build_type_index(
        pages: &[Vec<u8>],
        buffer: &HashMap<Vec<u8>, KVEntry>,
        hash_algo: HashAlgorithm,
    ) -> HashMap<u8, HashMap<Vec<u8>, KVEntry>> {
        let hash_length = hash_algo.hash_length();
        // Collect all entries from pages, deduplicating by hash
        let mut by_hash: HashMap<Vec<u8>, KVEntry> = HashMap::new();
        for page_data in pages.iter() {
            if let Ok(entries) = deserialize_page(page_data, hash_length) {
                for entry in entries {
                    by_hash.insert(entry.hash.clone(), entry);
                }
            }
        }

        // Buffer takes priority
        for (hash, entry) in buffer {
            by_hash.insert(hash.clone(), entry.clone());
        }

        // Group by type, excluding deleted
        let mut index: HashMap<u8, HashMap<Vec<u8>, KVEntry>> = HashMap::new();
        for (hash, entry) in by_hash {
            if (entry.type_flags & KV_FLAG_DELETED) != 0 {
                continue;
            }
            index.entry(entry.entry_type())
                .or_default()
                .insert(hash, entry);
        }

        index
    }

    /// Access the shared pages Arc (for cheap cloning in buffer-only publishes).
    pub fn pages(&self) -> &Arc<Vec<Vec<u8>>> {
        &self.pages
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

        let entries = deserialize_page(page_data, hash_length).ok()?;

        if include_deleted {
            entries.iter().find(|e| e.hash == hash).cloned()
        } else {
            find_in_page(&entries, hash).cloned()
        }
    }

    /// Iterate all entries of a specific type. O(k) where k is the number of
    /// entries of that type, backed by the prebuilt type index.
    pub fn iter_by_type(&self, target_type: u8) -> Vec<KVEntry> {
        match self.type_index.get(&target_type) {
            Some(entries) => entries.values().cloned().collect(),
            None => Vec::new(),
        }
    }

    /// Iterate all entries: uses the prebuilt type index to collect every
    /// non-deleted entry across all types. Still O(n) but avoids re-scanning
    /// pages and rebuilding the HashMap on every call.
    pub fn iter_all(&self) -> EngineResult<Vec<KVEntry>> {
        let mut all = Vec::new();
        for entries in self.type_index.values() {
            all.extend(entries.values().cloned());
        }
        Ok(all)
    }

    /// Check if an entry is marked as deleted in the buffer.
    pub fn is_deleted_in_buffer(&self, hash: &[u8]) -> bool {
        self.buffer
            .get(hash)
            .map(|e| (e.type_flags & KV_FLAG_DELETED) != 0)
            .unwrap_or(false)
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
