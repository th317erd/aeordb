use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::kv_pages::*;
use crate::engine::kv_snapshot::ReadSnapshot;
use crate::engine::kv_store::{KVEntry, KV_FLAG_DELETED};
use crate::engine::nvt::NormalizedVectorTable;
use crate::engine::scalar_converter::HashConverter;

/// Number of buffered writes before auto-flush to disk.
const WRITE_BUFFER_THRESHOLD: usize = 512;

/// Number of entries buffered before flushing to the hot file.
const HOT_BUFFER_THRESHOLD: usize = 10;

/// A disk-resident KV store backed by NVT-indexed bucket pages.
///
/// The KV data lives in a separate `.kv` file. Lookups flow through:
/// write_buffer -> NVT bucket -> disk page scan.
pub struct DiskKVStore {
    /// NVT for O(1) bucket lookup from hash bytes.
    nvt: NormalizedVectorTable,
    /// Write buffer: absorbs recent inserts before flushing to disk.
    write_buffer: HashMap<Vec<u8>, KVEntry>,
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
    /// Write-ahead journal file for crash recovery. None = disabled (tests).
    hot_file: Option<File>,
    /// Path to the hot file on disk.
    hot_path: Option<PathBuf>,
    /// Micro-buffer of entries pending write to the hot file.
    hot_buffer: Vec<KVEntry>,
    /// Shared snapshot for lock-free readers. Updated after every mutation.
    snapshot: Arc<ArcSwap<ReadSnapshot>>,
    /// Shared NVT wrapped in Arc — re-cloned only on flush/resize.
    shared_nvt: Arc<NormalizedVectorTable>,
}

impl DiskKVStore {
    /// Create a new disk KV store at the given path.
    /// Writes empty pages for stage 0.
    /// When `hot_dir` is Some, a write-ahead hot file is created for crash recovery.
    pub fn create(path: &Path, hash_algo: HashAlgorithm, hot_dir: Option<&Path>) -> EngineResult<Self> {
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

        let (hot_file, hot_path) = if let Some(dir) = hot_dir {
            // Derive db_name: "test.aeordb.kv" → stem "test.aeordb" → stem "test"
            let kv_stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("db");
            let db_name = std::path::Path::new(kv_stem)
                .file_stem().and_then(|s| s.to_str()).unwrap_or(kv_stem);
            let (f, p) = Self::init_hot_file(dir, db_name);
            (Some(f), Some(p))
        } else {
            (None, None)
        };

        let shared_nvt = Arc::new(nvt.clone());
        let kv_path = path.to_path_buf();
        let pages = Arc::new(vec![vec![0u8; page_size(hash_length)]; bucket_count]);
        let initial_snapshot = ReadSnapshot::new(
            HashMap::new(),
            Arc::clone(&shared_nvt),
            bucket_count,
            hash_algo,
            0,
            pages,
        );
        let snapshot = Arc::new(ArcSwap::new(Arc::new(initial_snapshot)));

        Ok(DiskKVStore {
            nvt,
            write_buffer: HashMap::new(),
            kv_file: file,
            kv_path,
            stage,
            hash_algo,
            entry_count: 0,
            bucket_count,
            hot_file,
            hot_path,
            hot_buffer: Vec::new(),
            snapshot,
            shared_nvt,
        })
    }

    /// Create a new disk KV store at the given path with a specific stage.
    /// Used during resize operations. No hot file — callers manage their own journaling.
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

        let shared_nvt = Arc::new(nvt.clone());
        let kv_path = path.to_path_buf();
        let pages = Arc::new(vec![vec![0u8; page_size(hash_length)]; bucket_count]);
        let initial_snapshot = ReadSnapshot::new(
            HashMap::new(),
            Arc::clone(&shared_nvt),
            bucket_count,
            hash_algo,
            0,
            pages,
        );
        let snapshot = Arc::new(ArcSwap::new(Arc::new(initial_snapshot)));

        Ok(DiskKVStore {
            nvt,
            write_buffer: HashMap::new(),
            kv_file: file,
            kv_path,
            stage,
            hash_algo,
            entry_count: 0,
            bucket_count,
            hot_file: None,
            hot_path: None,
            hot_buffer: Vec::new(),
            snapshot,
            shared_nvt,
        })
    }

    /// Open an existing disk KV store from a `.kv` file.
    /// Rebuilds entry count by scanning page headers.
    /// When `hot_dir` is Some, a write-ahead hot file is created for crash recovery.
    pub fn open(path: &Path, hash_algo: HashAlgorithm, hot_dir: Option<&Path>) -> EngineResult<Self> {
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

        let (hot_file, hot_path) = if let Some(dir) = hot_dir {
            // Derive db_name: "test.aeordb.kv" → stem "test.aeordb" → stem "test"
            let kv_stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("db");
            let db_name = std::path::Path::new(kv_stem)
                .file_stem().and_then(|s| s.to_str()).unwrap_or(kv_stem);
            let (f, p) = Self::init_hot_file(dir, db_name);
            (Some(f), Some(p))
        } else {
            (None, None)
        };

        let shared_nvt = Arc::new(nvt.clone());
        let kv_path = path.to_path_buf();
        // Read all pages for initial snapshot
        let pages = {
            let psize = page_size(hash_length);
            let mut pages = Vec::with_capacity(bucket_count);
            for bucket in 0..bucket_count {
                let offset = bucket_page_offset(bucket, hash_length);
                let mut page_data = vec![0u8; psize];
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(&mut page_data)?;
                pages.push(page_data);
            }
            Arc::new(pages)
        };
        let initial_snapshot = ReadSnapshot::new(
            HashMap::new(),
            Arc::clone(&shared_nvt),
            bucket_count,
            hash_algo,
            entry_count,
            pages,
        );
        let snapshot = Arc::new(ArcSwap::new(Arc::new(initial_snapshot)));

        Ok(DiskKVStore {
            nvt,
            write_buffer: HashMap::new(),
            kv_file: file,
            kv_path,
            stage,
            hash_algo,
            entry_count,
            bucket_count,
            hot_file,
            hot_path,
            hot_buffer: Vec::new(),
            snapshot,
            shared_nvt,
        })
    }

    /// Look up an entry by hash.
    /// Search order: write_buffer -> disk page.
    pub fn get(&mut self, hash: &[u8]) -> Option<KVEntry> {
        // 1. Check write buffer first (most recent writes)
        if let Some(entry) = self.write_buffer.get(hash) {
            if entry.is_deleted() {
                return None;
            }
            return Some(entry.clone());
        }

        // 2. Read from disk via NVT bucket mapping
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

        Some(found)
    }

    /// Insert or update an entry in the write buffer.
    /// Auto-flushes when the buffer exceeds WRITE_BUFFER_THRESHOLD.
    /// Also journals the entry to the hot file buffer for crash recovery.
    pub fn insert(&mut self, entry: KVEntry) {
        let is_new = !self.write_buffer.contains_key(&entry.hash)
            && !self.entry_exists_on_disk(&entry.hash);

        self.write_buffer.insert(entry.hash.clone(), entry.clone());

        if is_new {
            self.entry_count += 1;
        }

        // Journal to hot file buffer
        if self.hot_file.is_some() {
            self.hot_buffer.push(entry);
            if self.hot_buffer.len() >= HOT_BUFFER_THRESHOLD {
                let _ = self.flush_hot_buffer();
            }
        }

        let did_flush = if self.write_buffer.len() >= WRITE_BUFFER_THRESHOLD {
            let _ = self.flush();
            true
        } else {
            false
        };

        if !did_flush {
            self.publish_buffer_only();
        }
    }

    /// Bulk insert entries without snapshot publishing, hot file journaling,
    /// or disk dedup checks. Used during resize where all entries are known-unique
    /// and the store is a temporary target. Call `flush()` after all inserts.
    pub fn bulk_insert(&mut self, entries: &[KVEntry]) {
        for entry in entries {
            self.write_buffer.insert(entry.hash.clone(), entry.clone());
            self.entry_count += 1;

            if self.write_buffer.len() >= WRITE_BUFFER_THRESHOLD {
                let _ = self.flush_no_snapshot();
            }
        }
    }

    /// Flush write buffer to disk without publishing a snapshot.
    /// Used by `bulk_insert` during resize to avoid O(entries × pages) I/O.
    fn flush_no_snapshot(&mut self) -> EngineResult<()> {
        if self.write_buffer.is_empty() {
            return Ok(());
        }

        let hash_length = self.hash_algo.hash_length();
        let mut overflow_entries: Vec<KVEntry> = Vec::new();

        let buffer_entries: Vec<KVEntry> = self.write_buffer.values().cloned().collect();
        let mut by_bucket: HashMap<usize, Vec<KVEntry>> = HashMap::new();
        for entry in buffer_entries {
            let bucket = self.nvt.bucket_for_value(&entry.hash);
            by_bucket.entry(bucket).or_default().push(entry);
        }

        for (bucket_index, new_entries) in by_bucket {
            let offset = bucket_page_offset(bucket_index, hash_length);
            let psize = page_size(hash_length);

            let mut page_data = vec![0u8; psize];
            self.kv_file.seek(SeekFrom::Start(offset))?;
            self.kv_file.read_exact(&mut page_data)?;

            let mut existing = deserialize_page(&page_data, hash_length)?;

            for entry in new_entries {
                if !upsert_in_page(&mut existing, entry.clone()) {
                    overflow_entries.push(entry);
                }
            }

            let serialized = serialize_page(&existing, hash_length);
            self.kv_file.seek(SeekFrom::Start(offset))?;
            self.kv_file.write_all(&serialized)?;
        }

        self.kv_file.sync_data()?;
        self.write_buffer.clear();

        if !overflow_entries.is_empty() {
            // During bulk insert into a temp store, overflow shouldn't happen
            // (the store was created at the right stage). If it does, just
            // put them back in the buffer for the caller to handle.
            for entry in overflow_entries {
                self.write_buffer.insert(entry.hash.clone(), entry);
            }
        }

        Ok(())
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
    /// If any bucket page overflows (> MAX_ENTRIES_PER_PAGE), automatically
    /// resizes to the next stage and retries the flush.
    pub fn flush(&mut self) -> EngineResult<()> {
        if self.write_buffer.is_empty() {
            return Ok(());
        }

        let hash_length = self.hash_algo.hash_length();
        let mut overflow_entries: Vec<KVEntry> = Vec::new();

        // Collect entries to flush WITHOUT draining the buffer yet.
        // The buffer stays intact so concurrent readers (via snapshot) can
        // still find these entries while we're writing disk pages.
        let buffer_entries: Vec<KVEntry> = self.write_buffer.values().cloned().collect();
        let mut by_bucket: HashMap<usize, Vec<KVEntry>> = HashMap::new();
        for entry in buffer_entries {
            let bucket = self.nvt.bucket_for_value(&entry.hash);
            by_bucket.entry(bucket).or_default().push(entry);
        }

        // Track which buckets are modified for incremental snapshot publishing
        let modified_buckets: Vec<usize> = by_bucket.keys().cloned().collect();

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
                if !upsert_in_page(&mut existing, entry.clone()) {
                    overflow_entries.push(entry);
                }
            }

            // Write merged page back
            let serialized = serialize_page(&existing, hash_length);
            self.kv_file.seek(SeekFrom::Start(offset))?;
            self.kv_file.write_all(&serialized)?;
        }

        self.kv_file.sync_data()?;

        // NOW drain the buffer — disk pages are stable, safe for readers.
        self.write_buffer.clear();

        // Hot file: flush remaining buffer, then truncate (all data is on KV pages now)
        self.flush_hot_buffer()?;
        self.truncate_hot_file()?;

        // Incremental snapshot publish — only re-read modified pages from disk.
        // Unmodified pages are shared via Arc from the previous snapshot.
        self.publish_snapshot_incremental(&modified_buckets);

        // Handle overflows: resize to next stage and retry
        // resize_to_next_stage() does a full page re-read since ALL pages change.
        if !overflow_entries.is_empty() {
            self.resize_to_next_stage()?;
            for entry in overflow_entries {
                self.write_buffer.insert(entry.hash.clone(), entry);
            }
            return self.flush();
        }

        Ok(())
    }

    /// Resize the KV store to the next stage (more buckets, larger file).
    /// Reads all entries from the current file, creates a new file at the
    /// next stage, inserts all non-deleted entries, and swaps the files.
    pub fn resize_to_next_stage(&mut self) -> EngineResult<()> {
        let new_stage = (self.stage + 1).min(KV_STAGES.len() - 1);
        if new_stage == self.stage {
            return Err(EngineError::IoError(std::io::Error::other(
                "KV store at maximum stage — cannot resize further",
            )));
        }

        // Read all non-deleted entries from current file
        let all_entries = self.iter_all()?;

        // Create new store at a temp path with the next stage
        let temp_path = self.kv_path.with_extension("kv.resize");
        let _ = std::fs::remove_file(&temp_path);

        let mut new_store = DiskKVStore::create_at_stage(
            &temp_path, self.hash_algo, new_stage,
        )?;

        // Bulk insert all entries (no snapshot publishing, no hot file, no dedup checks)
        new_store.bulk_insert(&all_entries);
        new_store.flush_no_snapshot()?;

        // Drop the new store (closes its file handle) so the temp file
        // can be renamed on all platforms. Clear its write buffer first
        // to prevent the Drop impl from trying to flush.
        new_store.write_buffer.clear();
        drop(new_store);

        // Swap files: rename temp -> current path
        std::fs::rename(&temp_path, &self.kv_path)?;

        // Reopen at the original path
        self.kv_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.kv_path)?;

        // Update internal state
        self.stage = new_stage;
        self.bucket_count = KV_STAGES[new_stage].1;
        self.nvt = NormalizedVectorTable::new(
            Box::new(HashConverter), self.bucket_count,
        );
        self.entry_count = all_entries.len();

        // Publish full snapshot with fresh NVT clone (bucket layout changed — ALL pages differ)
        self.publish_full_snapshot_with_new_nvt();

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
            self.write_buffer.insert(hash.to_vec(), entry);
            self.entry_count = self.entry_count.saturating_sub(1);
            self.publish_buffer_only();
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
            self.write_buffer.insert(hash.to_vec(), entry);
            self.publish_buffer_only();
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
            self.write_buffer.insert(hash.to_vec(), entry);
            self.publish_buffer_only();
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

    // ========================================================================
    // Snapshot publishing methods
    // ========================================================================

    /// Read all KV pages into memory for a snapshot.
    /// Returns an Arc-wrapped Vec of page data, one per bucket.
    fn read_all_pages(&mut self) -> Arc<Vec<Vec<u8>>> {
        let hash_length = self.hash_algo.hash_length();
        let psize = page_size(hash_length);
        let mut pages = Vec::with_capacity(self.bucket_count);

        for bucket in 0..self.bucket_count {
            let offset = bucket_page_offset(bucket, hash_length);
            let mut page_data = vec![0u8; psize];
            if self.kv_file.seek(SeekFrom::Start(offset)).is_ok() {
                if self.kv_file.read_exact(&mut page_data).is_ok() {
                    pages.push(page_data);
                    continue;
                }
            }
            // If read fails, push empty page
            pages.push(vec![0u8; psize]);
        }

        Arc::new(pages)
    }

    /// Cheap publish: clone buffer + reuse existing pages (Arc clone = atomic op).
    /// Called on every insert/mutation. Does NOT read pages from disk.
    fn publish_buffer_only(&mut self) {
        let current_pages = {
            let current = self.snapshot.load();
            Arc::clone(current.pages())
        };
        let snapshot = ReadSnapshot::new(
            self.write_buffer.clone(),
            Arc::clone(&self.shared_nvt),
            self.bucket_count,
            self.hash_algo,
            self.entry_count,
            current_pages,
        );
        self.snapshot.store(Arc::new(snapshot));
    }

    /// Publish a full snapshot by re-reading ALL pages from disk.
    /// Used after resize_to_next_stage where all pages change.
    fn publish_full_snapshot(&mut self) {
        let pages = self.read_all_pages();
        let snapshot = ReadSnapshot::new(
            self.write_buffer.clone(),
            Arc::clone(&self.shared_nvt),
            self.bucket_count,
            self.hash_algo,
            self.entry_count,
            pages,
        );
        self.snapshot.store(Arc::new(snapshot));
    }

    /// Incremental snapshot publish: only re-read modified pages from disk.
    /// Unmodified pages are shared via Arc from the previous snapshot.
    fn publish_snapshot_incremental(&mut self, modified_buckets: &[usize]) {
        self.shared_nvt = Arc::new(self.nvt.clone());

        // Get current pages from existing snapshot
        let current = self.snapshot.load();
        let old_pages = current.pages();

        // Clone the outer Vec (just pointers), but share unmodified inner Vecs
        let mut new_pages = (**old_pages).clone();

        // Only read modified pages from disk
        let hash_length = self.hash_algo.hash_length();
        let psize = page_size(hash_length);
        for &bucket in modified_buckets {
            if bucket < new_pages.len() {
                let offset = bucket_page_offset(bucket, hash_length);
                let mut page_data = vec![0u8; psize];
                if self.kv_file.seek(SeekFrom::Start(offset)).is_ok() {
                    let _ = self.kv_file.read_exact(&mut page_data);
                }
                new_pages[bucket] = page_data;
            }
        }

        let snapshot = ReadSnapshot::new(
            self.write_buffer.clone(),
            Arc::clone(&self.shared_nvt),
            self.bucket_count,
            self.hash_algo,
            self.entry_count,
            Arc::new(new_pages),
        );
        self.snapshot.store(Arc::new(snapshot));
    }

    /// Publish a full snapshot with a fresh NVT clone (called after resize).
    fn publish_full_snapshot_with_new_nvt(&mut self) {
        self.shared_nvt = Arc::new(self.nvt.clone());
        self.publish_full_snapshot();
    }

    /// Get a reference to the ArcSwap for readers to load snapshots from.
    pub fn snapshot_handle(&self) -> &Arc<ArcSwap<ReadSnapshot>> {
        &self.snapshot
    }

    // ========================================================================
    // Hot file (write-ahead journal) methods
    // ========================================================================

    /// Initialize the hot file. Called during create/open.
    /// Panics (via process::exit) if the file cannot be opened.
    fn init_hot_file(hot_dir: &Path, db_name: &str) -> (File, PathBuf) {
        let hot_name = format!("{}-hot001", db_name);
        let hot_path = hot_dir.join(hot_name);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&hot_path)
            .unwrap_or_else(|e| {
                eprintln!("FATAL: Cannot open hot file at {}: {}", hot_path.display(), e);
                eprintln!("The database cannot start without a hot file for crash recovery.");
                eprintln!("Check permissions and disk space, or specify --hot-dir to use a different location.");
                std::process::exit(1);
            });

        (file, hot_path)
    }

    /// Flush the hot buffer to the hot file on disk.
    pub fn flush_hot_buffer(&mut self) -> EngineResult<()> {
        if self.hot_buffer.is_empty() || self.hot_file.is_none() {
            return Ok(());
        }

        let hash_length = self.hash_algo.hash_length();
        let entry_size = hash_length + 1 + 8;

        if let Some(ref mut file) = self.hot_file {
            for entry in &self.hot_buffer {
                let mut buf = Vec::with_capacity(entry_size);
                let hash_bytes = &entry.hash;
                buf.extend_from_slice(&hash_bytes[..hash_length.min(hash_bytes.len())]);
                // Pad if hash is shorter than hash_length
                if hash_bytes.len() < hash_length {
                    buf.extend(std::iter::repeat(0u8).take(hash_length - hash_bytes.len()));
                }
                buf.push(entry.type_flags);
                buf.extend_from_slice(&entry.offset.to_le_bytes());
                file.write_all(&buf)?;
            }
            file.sync_data()?;
            self.hot_buffer.clear();
        }

        Ok(())
    }

    /// Read entries from a hot file on disk.
    pub fn read_hot_file(path: &Path, hash_length: usize) -> EngineResult<Vec<KVEntry>> {
        let mut file = File::open(path)?;
        let file_size = file.metadata()?.len() as usize;
        let entry_size = hash_length + 1 + 8;

        if file_size == 0 {
            return Ok(Vec::new());
        }

        let entry_count = file_size / entry_size;
        let mut entries = Vec::with_capacity(entry_count);

        let mut buf = vec![0u8; entry_size];
        for _ in 0..entry_count {
            if file.read_exact(&mut buf).is_err() {
                break; // truncated entry at end — skip
            }
            let hash = buf[..hash_length].to_vec();
            let type_flags = buf[hash_length];
            let offset = u64::from_le_bytes(buf[hash_length + 1..hash_length + 9].try_into().unwrap());
            entries.push(KVEntry { type_flags, hash, offset });
        }

        Ok(entries)
    }

    /// Truncate the hot file (after successful KV flush).
    fn truncate_hot_file(&mut self) -> EngineResult<()> {
        if let Some(ref mut file) = self.hot_file {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
        }
        Ok(())
    }

    /// Return the path to the hot file, if one is configured.
    pub fn hot_path(&self) -> Option<&Path> {
        self.hot_path.as_deref()
    }

    /// Return the number of entries currently in the hot buffer (for testing).
    pub fn hot_buffer_len(&self) -> usize {
        self.hot_buffer.len()
    }

}

impl Drop for DiskKVStore {
    fn drop(&mut self) {
        // Best-effort flush of any remaining write buffer entries to disk.
        let _ = self.flush();
    }
}
