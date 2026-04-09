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
/// memory — no disk I/O, no race conditions with concurrent writers.
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
    pages: Vec<Vec<u8>>,
}

impl fmt::Debug for ReadSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadSnapshot")
            .field("bucket_count", &self.bucket_count)
            .field("hash_algo", &self.hash_algo)
            .field("entry_count", &self.entry_count)
            .field("buffer_len", &self.buffer.len())
            .field("pages", &format_args!("Vec<{} pages>", self.pages.len()))
            .finish_non_exhaustive()
    }
}

impl ReadSnapshot {
    /// Create a new read snapshot from a frozen buffer, shared NVT, and an
    /// in-memory copy of all KV pages.
    pub fn new(
        buffer: HashMap<Vec<u8>, KVEntry>,
        nvt: Arc<NormalizedVectorTable>,
        bucket_count: usize,
        hash_algo: HashAlgorithm,
        entry_count: usize,
        pages: Vec<Vec<u8>>,
    ) -> Self {
        ReadSnapshot {
            buffer,
            nvt,
            bucket_count,
            hash_algo,
            entry_count,
            pages,
        }
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

    /// Iterate all entries: reads every page from in-memory cache, merges with buffer,
    /// excludes deleted entries.
    pub fn iter_all(&self) -> EngineResult<Vec<KVEntry>> {
        let hash_length = self.hash_algo.hash_length();
        let mut all: HashMap<Vec<u8>, KVEntry> = HashMap::new();

        // Read all pages from in-memory cache
        for page_data in &self.pages {
            if let Ok(entries) = deserialize_page(page_data, hash_length) {
                for entry in entries {
                    all.insert(entry.hash.clone(), entry);
                }
            }
        }

        // Merge buffer (buffer takes priority)
        for (hash, entry) in &self.buffer {
            all.insert(hash.clone(), entry.clone());
        }

        // Filter out deleted entries
        Ok(all
            .into_values()
            .filter(|e| (e.type_flags & KV_FLAG_DELETED) == 0)
            .collect())
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
