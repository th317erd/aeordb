use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::kv_pages::*;
use crate::engine::kv_store::{KVEntry, KV_FLAG_DELETED};
use crate::engine::nvt::NormalizedVectorTable;
use crate::engine::scalar_converter::HashConverter;

/// Number of buffered writes before auto-flush to disk.
const WRITE_BUFFER_THRESHOLD: usize = 1000;

/// Maximum entries in the hot cache (simple LRU eviction).
const HOT_CACHE_MAX: usize = 10_000;

/// A disk-resident KV store backed by NVT-indexed bucket pages.
///
/// The KV data lives in a separate `.kv` file. Lookups flow through:
/// write_buffer -> hot_cache -> NVT bucket -> disk page scan.
pub struct DiskKVStore {
    /// NVT for O(1) bucket lookup from hash bytes.
    nvt: NormalizedVectorTable,
    /// Write buffer: absorbs recent inserts before flushing to disk.
    write_buffer: HashMap<Vec<u8>, KVEntry>,
    /// Hot cache: recently read entries from disk.
    hot_cache: HashMap<Vec<u8>, KVEntry>,
    /// LRU order tracking for hot cache eviction (oldest first).
    cache_order: Vec<Vec<u8>>,
    /// File handle for the KV pages on disk.
    kv_file: File,
    /// Path to the KV file.
    kv_path: PathBuf,
    /// Current stage in the KV_STAGES table.
    stage: usize,
    /// Hash algorithm (determines hash_length for page layout).
    hash_algo: HashAlgorithm,
    /// Total entry count (disk + buffer, minus deleted).
    entry_count: usize,
    /// Number of buckets at the current stage.
    bucket_count: usize,
}

impl DiskKVStore {
    /// Create a new disk KV store at the given path.
    /// Writes empty pages for stage 0.
    pub fn create(path: &Path, hash_algo: HashAlgorithm) -> EngineResult<Self> {
        let stage = 0;
        let (_block_size, bucket_count) = KV_STAGES[stage];
        let hash_length = hash_algo.hash_length();

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(EngineError::from)?;

        // Write empty pages for all buckets
        let empty_page = vec![0u8; page_size(hash_length)];
        for _ in 0..bucket_count {
            file.write_all(&empty_page)?;
        }
        file.sync_all()?;

        let nvt = NormalizedVectorTable::new(Box::new(HashConverter), bucket_count);

        Ok(DiskKVStore {
            nvt,
            write_buffer: HashMap::new(),
            hot_cache: HashMap::new(),
            cache_order: Vec::new(),
            kv_file: file,
            kv_path: path.to_path_buf(),
            stage,
            hash_algo,
            entry_count: 0,
            bucket_count,
        })
    }

    /// Create a new disk KV store at the given path with a specific stage.
    /// Used during resize operations.
    pub fn create_at_stage(
        path: &Path,
        hash_algo: HashAlgorithm,
        stage: usize,
    ) -> EngineResult<Self> {
        let stage = stage.min(KV_STAGES.len() - 1);
        let (_block_size, bucket_count) = KV_STAGES[stage];
        let hash_length = hash_algo.hash_length();

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(EngineError::from)?;

        let empty_page = vec![0u8; page_size(hash_length)];
        for _ in 0..bucket_count {
            file.write_all(&empty_page)?;
        }
        file.sync_all()?;

        let nvt = NormalizedVectorTable::new(Box::new(HashConverter), bucket_count);

        Ok(DiskKVStore {
            nvt,
            write_buffer: HashMap::new(),
            hot_cache: HashMap::new(),
            cache_order: Vec::new(),
            kv_file: file,
            kv_path: path.to_path_buf(),
            stage,
            hash_algo,
            entry_count: 0,
            bucket_count,
        })
    }

    /// Open an existing disk KV store from a `.kv` file.
    /// Rebuilds entry count by scanning page headers.
    pub fn open(path: &Path, hash_algo: HashAlgorithm) -> EngineResult<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(EngineError::from)?;

        let file_size = file.metadata()?.len();
        let hash_length = hash_algo.hash_length();
        let psize = page_size(hash_length) as u64;

        if psize == 0 {
            return Err(EngineError::CorruptEntry {
                offset: 0,
                reason: "Zero page size".to_string(),
            });
        }

        // Determine stage from file size: find the largest stage whose
        // bucket_count * page_size matches the file size.
        let mut stage = 0;
        let mut bucket_count = KV_STAGES[0].1;
        for (s, (_block_size, buckets)) in KV_STAGES.iter().enumerate() {
            let expected_size = *buckets as u64 * psize;
            if file_size >= expected_size {
                stage = s;
                bucket_count = *buckets;
            }
        }

        // Rebuild entry count by reading each page header (2 bytes each)
        let mut entry_count = 0;
        let mut header_buf = [0u8; 2];
        for bucket in 0..bucket_count {
            let offset = bucket as u64 * psize;
            if offset + 2 > file_size {
                break;
            }
            file.seek(SeekFrom::Start(offset))?;
            if file.read_exact(&mut header_buf).is_ok() {
                let count = u16::from_le_bytes(header_buf) as usize;
                entry_count += count;
            }
        }

        let nvt = NormalizedVectorTable::new(Box::new(HashConverter), bucket_count);

        Ok(DiskKVStore {
            nvt,
            write_buffer: HashMap::new(),
            hot_cache: HashMap::new(),
            cache_order: Vec::new(),
            kv_file: file,
            kv_path: path.to_path_buf(),
            stage,
            hash_algo,
            entry_count,
            bucket_count,
        })
    }

    /// Look up an entry by hash.
    /// Search order: write_buffer -> hot_cache -> disk page.
    pub fn get(&mut self, hash: &[u8]) -> Option<KVEntry> {
        // 1. Check write buffer first (most recent writes)
        if let Some(entry) = self.write_buffer.get(hash) {
            if entry.is_deleted() {
                return None;
            }
            return Some(entry.clone());
        }

        // 2. Check hot cache
        if let Some(entry) = self.hot_cache.get(hash) {
            if entry.is_deleted() {
                return None;
            }
            return Some(entry.clone());
        }

        // 3. Read from disk via NVT bucket mapping
        let bucket_index = self.nvt.bucket_for_value(hash);
        let hash_length = self.hash_algo.hash_length();
        let offset = bucket_page_offset(bucket_index, hash_length);
        let psize = page_size(hash_length);

        let mut page_data = vec![0u8; psize];
        if self.kv_file.seek(SeekFrom::Start(offset)).is_err() {
            return None;
        }
        if self.kv_file.read_exact(&mut page_data).is_err() {
            return None;
        }

        let entries = deserialize_page(&page_data, hash_length).ok()?;
        let found = find_in_page(&entries, hash)?.clone();

        // Cache the result
        self.cache_put(hash, &found);

        Some(found)
    }

    /// Insert or update an entry in the write buffer.
    /// Auto-flushes when the buffer exceeds WRITE_BUFFER_THRESHOLD.
    pub fn insert(&mut self, entry: KVEntry) {
        let is_new = !self.write_buffer.contains_key(&entry.hash)
            && !self.entry_exists_on_disk(&entry.hash);

        // Invalidate hot cache for this hash
        self.hot_cache.remove(&entry.hash);
        self.cache_order.retain(|h| h != &entry.hash);

        self.write_buffer.insert(entry.hash.clone(), entry);

        if is_new {
            self.entry_count += 1;
        }

        if self.write_buffer.len() >= WRITE_BUFFER_THRESHOLD {
            let _ = self.flush();
        }
    }

    /// Check if an entry exists on disk (without caching).
    fn entry_exists_on_disk(&mut self, hash: &[u8]) -> bool {
        let bucket_index = self.nvt.bucket_for_value(hash);
        let hash_length = self.hash_algo.hash_length();
        let offset = bucket_page_offset(bucket_index, hash_length);
        let psize = page_size(hash_length);

        let mut page_data = vec![0u8; psize];
        if self.kv_file.seek(SeekFrom::Start(offset)).is_err() {
            return false;
        }
        if self.kv_file.read_exact(&mut page_data).is_err() {
            return false;
        }

        if let Ok(entries) = deserialize_page(&page_data, hash_length) {
            entries.iter().any(|e| e.hash == hash)
        } else {
            false
        }
    }

    /// Flush the write buffer to disk.
    /// Groups entries by bucket, reads each affected page, merges, writes back.
    pub fn flush(&mut self) -> EngineResult<()> {
        if self.write_buffer.is_empty() {
            return Ok(());
        }

        let hash_length = self.hash_algo.hash_length();

        // Group buffered entries by NVT bucket
        let mut by_bucket: HashMap<usize, Vec<KVEntry>> = HashMap::new();
        for (_hash, entry) in self.write_buffer.drain() {
            let bucket = self.nvt.bucket_for_value(&entry.hash);
            by_bucket.entry(bucket).or_default().push(entry);
        }

        // For each affected bucket: read page, merge, write back
        for (bucket_index, new_entries) in by_bucket {
            let offset = bucket_page_offset(bucket_index, hash_length);
            let psize = page_size(hash_length);

            // Read existing page
            let mut page_data = vec![0u8; psize];
            self.kv_file.seek(SeekFrom::Start(offset))?;
            self.kv_file.read_exact(&mut page_data)?;

            let mut existing = deserialize_page(&page_data, hash_length)?;

            // Merge new entries into existing page
            for entry in new_entries {
                if !upsert_in_page(&mut existing, entry) {
                    // Page full — would need resize (Task 5 handles this).
                    return Err(EngineError::IoError(std::io::Error::other(
                        "KV bucket page overflow — resize needed",
                    )));
                }
            }

            // Write merged page back
            let serialized = serialize_page(&existing, hash_length);
            self.kv_file.seek(SeekFrom::Start(offset))?;
            self.kv_file.write_all(&serialized)?;
        }

        self.kv_file.sync_data()?;
        Ok(())
    }

    /// Check if an entry exists by hash.
    pub fn contains(&mut self, hash: &[u8]) -> bool {
        self.get(hash).is_some()
    }

    /// Mark an entry as deleted by setting the KV_FLAG_DELETED flag.
    pub fn mark_deleted(&mut self, hash: &[u8]) {
        if let Some(mut entry) = self.get(hash) {
            entry.type_flags |= KV_FLAG_DELETED;
            self.hot_cache.remove(hash);
            self.cache_order.retain(|h| h != hash);
            self.write_buffer.insert(hash.to_vec(), entry);
            self.entry_count = self.entry_count.saturating_sub(1);
        }
    }

    /// Iterate all entries: reads every page from disk and merges with write buffer.
    /// Excludes deleted entries.
    pub fn iter_all(&mut self) -> EngineResult<Vec<KVEntry>> {
        let hash_length = self.hash_algo.hash_length();
        let psize = page_size(hash_length);
        let mut all: HashMap<Vec<u8>, KVEntry> = HashMap::new();

        // Read all pages from disk
        for bucket in 0..self.bucket_count {
            let offset = bucket_page_offset(bucket, hash_length);
            let mut page_data = vec![0u8; psize];
            self.kv_file.seek(SeekFrom::Start(offset))?;
            if self.kv_file.read_exact(&mut page_data).is_ok() {
                if let Ok(entries) = deserialize_page(&page_data, hash_length) {
                    for entry in entries {
                        all.insert(entry.hash.clone(), entry);
                    }
                }
            }
        }

        // Merge write buffer (buffer takes priority)
        for (hash, entry) in &self.write_buffer {
            all.insert(hash.clone(), entry.clone());
        }

        // Filter out deleted entries
        Ok(all
            .into_values()
            .filter(|e| !e.is_deleted())
            .collect())
    }

    /// Total entry count (non-deleted).
    pub fn len(&self) -> usize {
        self.entry_count
    }

    /// Whether the store has zero entries.
    pub fn is_empty(&self) -> bool {
        self.entry_count == 0
    }

    /// Update type_flags for an entry identified by hash.
    /// Returns true if the entry was found and updated.
    pub fn update_flags(&mut self, hash: &[u8], new_flags: u8) -> bool {
        if let Some(mut entry) = self.get(hash) {
            let entry_type = entry.type_flags & 0x0F;
            entry.type_flags = entry_type | (new_flags & 0xF0);
            self.hot_cache.remove(hash);
            self.cache_order.retain(|h| h != hash);
            self.write_buffer.insert(hash.to_vec(), entry);
            true
        } else {
            false
        }
    }

    /// Update file offset for an entry identified by hash.
    /// Returns true if the entry was found and updated.
    pub fn update_offset(&mut self, hash: &[u8], new_offset: u64) -> bool {
        if let Some(mut entry) = self.get(hash) {
            entry.offset = new_offset;
            self.hot_cache.remove(hash);
            self.cache_order.retain(|h| h != hash);
            self.write_buffer.insert(hash.to_vec(), entry);
            true
        } else {
            false
        }
    }

    /// Current stage index.
    pub fn stage(&self) -> usize {
        self.stage
    }

    /// Current bucket count.
    pub fn bucket_count(&self) -> usize {
        self.bucket_count
    }

    /// Path to the KV file.
    pub fn path(&self) -> &Path {
        &self.kv_path
    }

    /// Hash algorithm in use.
    pub fn hash_algo(&self) -> HashAlgorithm {
        self.hash_algo
    }

    /// Check if a hash exists in the hot cache.
    pub fn is_cached(&self, hash: &[u8]) -> bool {
        self.hot_cache.contains_key(hash)
    }

    /// Add an entry to the hot cache with LRU eviction.
    fn cache_put(&mut self, hash: &[u8], entry: &KVEntry) {
        // Remove existing entry from order tracking if present
        self.cache_order.retain(|h| h != hash);

        if self.hot_cache.len() >= HOT_CACHE_MAX && !self.hot_cache.contains_key(hash) {
            // Evict oldest entry
            if let Some(old_hash) = self.cache_order.first().cloned() {
                self.hot_cache.remove(&old_hash);
                self.cache_order.remove(0);
            }
        }

        self.hot_cache.insert(hash.to_vec(), entry.clone());
        self.cache_order.push(hash.to_vec());
    }
}
