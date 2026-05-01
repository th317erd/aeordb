use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::hot_tail;
use crate::engine::kv_pages::*;
use crate::engine::kv_snapshot::ReadSnapshot;
use crate::engine::kv_stages::{KV_STAGE_SIZES, stage_params};
use crate::engine::kv_store::{KVEntry, KV_FLAG_DELETED};
use crate::engine::nvt::NormalizedVectorTable;
use crate::engine::scalar_converter::HashConverter;

/// Number of buffered writes before auto-flush to KV bucket pages.
const WRITE_BUFFER_THRESHOLD: usize = 512;

/// Number of entries buffered before flushing to the hot tail.
const HOT_BUFFER_THRESHOLD: usize = 1_000;

/// A disk-resident KV store using NVT-indexed bucket pages inside the main
/// database file. No sidecar files — the KV block lives at the head of the
/// .aeordb file and the hot tail dangles off the end.
///
/// Lookup flow: write_buffer → NVT bucket → disk page scan.
pub struct DiskKVStore {
    /// NVT for O(1) bucket lookup from hash bytes.
    nvt: NormalizedVectorTable,
    /// Write buffer: absorbs recent inserts before flushing to disk.
    write_buffer: HashMap<Vec<u8>, KVEntry>,
    /// File handle for the main .aeordb database file.
    /// KV pages are at kv_block_offset; hot tail at hot_tail_offset.
    db_file: File,
    /// Offset of the KV block within the database file.
    kv_block_offset: u64,
    /// Size of the KV block in bytes (pages must fit within this).
    kv_block_length: u64,
    /// Offset of the hot tail within the database file.
    hot_tail_offset: u64,
    /// Whether the hot tail is enabled (false for temp stores during resize).
    hot_tail_enabled: bool,
    /// Current stage in the KV_STAGE_SIZES table.
    stage: usize,
    /// Hash algorithm (determines hash_length for page layout).
    hash_algo: HashAlgorithm,
    /// Total entry count (disk + buffer, minus deleted).
    entry_count: usize,
    /// Number of buckets at the current stage.
    bucket_count: usize,
    /// Micro-buffer of entries pending write to the hot tail.
    hot_buffer: Vec<KVEntry>,
    /// Shared snapshot for lock-free readers. Updated after every mutation.
    snapshot: Arc<ArcSwap<ReadSnapshot>>,
    /// Shared NVT wrapped in Arc — re-cloned only on flush/resize.
    shared_nvt: Arc<NormalizedVectorTable>,
    /// Set to true when a corrupt KV page is detected and zeroed during flush.
    pub needs_rebuild: bool,
    /// Transaction nesting depth. When > 0, flush() skips clearing the hot tail.
    pub transaction_depth: u32,
}

impl DiskKVStore {
    /// Create a new in-file KV store. Writes empty bucket pages at kv_block_offset.
    ///
    /// `db_file` is a clone of the main .aeordb file handle.
    /// `kv_block_offset` is where the KV block starts (typically 256, after file header).
    /// `hot_tail_offset` is where the hot tail lives (end of the file).
    pub fn create(
        mut db_file: File,
        hash_algo: HashAlgorithm,
        kv_block_offset: u64,
        hot_tail_offset: u64,
        stage: usize,
    ) -> EngineResult<Self> {
        let stage = stage.min(KV_STAGE_SIZES.len() - 1);
        let hash_length = hash_algo.hash_length();
        let psize = page_size(hash_length);
        let (block_size, bucket_count) = stage_params(stage, psize);
        // kv_block_length is the stage's block size — NOT the distance to hot_tail.
        // The WAL entries sit between the KV block and the hot tail.
        let kv_block_length = block_size;

        tracing::debug!(
            kv_block_offset, kv_block_length, hot_tail_offset, stage, bucket_count, psize,
            pages_bytes = bucket_count * psize,
            max_entries = bucket_count * MAX_ENTRIES_PER_PAGE,
            "DiskKVStore::create"
        );

        // Write empty pages for all buckets at kv_block_offset
        let empty_page = vec![0u8; psize];
        db_file.seek(SeekFrom::Start(kv_block_offset))?;
        for _ in 0..bucket_count {
            db_file.write_all(&empty_page)?;
        }
        db_file.sync_data()?;

        let nvt = NormalizedVectorTable::new(Box::new(HashConverter), bucket_count);
        let shared_nvt = Arc::new(nvt.clone());
        let pages = Arc::new(vec![vec![0u8; psize]; bucket_count]);
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
            db_file,
            kv_block_offset,
            kv_block_length,
            hot_tail_offset,
            hot_tail_enabled: true,
            stage,
            hash_algo,
            entry_count: 0,
            bucket_count,
            hot_buffer: Vec::new(),
            snapshot,
            shared_nvt,
            needs_rebuild: false,
            transaction_depth: 0,
        })
    }

    /// Open an existing in-file KV store by reading bucket pages from the database file.
    ///
    /// `db_file` is a clone of the main .aeordb file handle.
    /// `kv_block_offset` and `hot_tail_offset` come from the file header.
    /// `stage` comes from the file header's `kv_block_stage`.
    /// `hot_entries` are entries loaded from the hot tail (passed in by StorageEngine).
    pub fn open(
        mut db_file: File,
        hash_algo: HashAlgorithm,
        kv_block_offset: u64,
        hot_tail_offset: u64,
        stage: usize,
        hot_entries: Vec<KVEntry>,
    ) -> EngineResult<Self> {
        let hot_entry_count = hot_entries.len();
        let stage = stage.min(KV_STAGE_SIZES.len() - 1);
        let hash_length = hash_algo.hash_length();
        let psize = page_size(hash_length);
        let (block_size, bucket_count) = stage_params(stage, psize);
        let kv_block_length = block_size;

        tracing::debug!(
            kv_block_offset, kv_block_length, hot_tail_offset, stage, bucket_count,
            hot_entry_count,
            "DiskKVStore::open"
        );

        // Rebuild entry count by reading each page header
        let mut entry_count = 0;
        let mut header_buf = [0u8; 2];
        for bucket in 0..bucket_count {
            let offset = kv_block_offset + bucket_page_offset(bucket, hash_length);
            db_file.seek(SeekFrom::Start(offset))?;
            if db_file.read_exact(&mut header_buf).is_ok() {
                entry_count += u16::from_le_bytes(header_buf) as usize;
            }
        }

        let nvt = NormalizedVectorTable::new(Box::new(HashConverter), bucket_count);
        let shared_nvt = Arc::new(nvt.clone());

        // Read all pages for initial snapshot
        let pages = {
            let mut pages = Vec::with_capacity(bucket_count);
            for bucket in 0..bucket_count {
                let offset = kv_block_offset + bucket_page_offset(bucket, hash_length);
                let mut page_data = vec![0u8; psize];
                db_file.seek(SeekFrom::Start(offset))?;
                db_file.read_exact(&mut page_data)?;
                pages.push(page_data);
            }
            Arc::new(pages)
        };

        // Pre-populate write buffer with hot entries (not yet flushed to pages)
        let mut write_buffer = HashMap::new();
        for entry in hot_entries {
            write_buffer.insert(entry.hash.clone(), entry);
        }
        let hot_count = write_buffer.len();

        let initial_snapshot = ReadSnapshot::new(
            write_buffer.clone(),
            Arc::clone(&shared_nvt),
            bucket_count,
            hash_algo,
            entry_count + hot_count,
            pages,
        );
        let snapshot = Arc::new(ArcSwap::new(Arc::new(initial_snapshot)));

        Ok(DiskKVStore {
            nvt,
            write_buffer,
            db_file,
            kv_block_offset,
            kv_block_length,
            hot_tail_offset,
            hot_tail_enabled: true,
            stage,
            hash_algo,
            entry_count: entry_count + hot_count,
            bucket_count,
            hot_buffer: Vec::new(),
            snapshot,
            shared_nvt,
            needs_rebuild: false,
            transaction_depth: 0,
        })
    }

    /// Create a temporary KV store for resize operations. No hot tail.
    pub fn create_temp(
        db_file: File,
        hash_algo: HashAlgorithm,
        kv_block_offset: u64,
        stage: usize,
    ) -> EngineResult<Self> {
        let mut store = Self::create(db_file, hash_algo, kv_block_offset, 0, stage)?;
        store.hot_tail_enabled = false;
        Ok(store)
    }

    // ========================================================================
    // Core KV operations
    // ========================================================================

    /// Look up an entry by hash.
    /// Search order: write_buffer → disk page.
    pub fn get(&mut self, hash: &[u8]) -> Option<KVEntry> {
        if let Some(entry) = self.write_buffer.get(hash) {
            if entry.is_deleted() { return None; }
            return Some(entry.clone());
        }

        let bucket_index = self.nvt.bucket_for_value(hash);
        let hash_length = self.hash_algo.hash_length();
        let offset = self.kv_block_offset + bucket_page_offset(bucket_index, hash_length);
        let psize = page_size(hash_length);

        let mut page_data = vec![0u8; psize];
        if self.db_file.seek(SeekFrom::Start(offset)).is_err() { return None; }
        if self.db_file.read_exact(&mut page_data).is_err() { return None; }

        let entries = deserialize_page(&page_data, hash_length).ok()?;
        let found = find_in_page(&entries, hash)?.clone();
        Some(found)
    }

    /// Insert or update an entry.
    pub fn insert(&mut self, entry: KVEntry) -> EngineResult<()> {
        let is_new = !self.write_buffer.contains_key(&entry.hash)
            && !self.entry_exists_on_disk(&entry.hash);

        self.write_buffer.insert(entry.hash.clone(), entry.clone());

        if is_new {
            self.entry_count += 1;
        }

        // Journal to hot buffer
        if self.hot_tail_enabled {
            self.hot_buffer.push(entry);
            if self.hot_buffer.len() >= HOT_BUFFER_THRESHOLD {
                self.flush_hot_buffer()?;
            }
        }

        let did_flush = if self.write_buffer.len() >= WRITE_BUFFER_THRESHOLD {
            self.flush()?;
            true
        } else {
            false
        };

        if !did_flush {
            self.publish_buffer_only();
        }

        Ok(())
    }

    /// Bulk insert without snapshot publishing or hot journaling.
    pub fn bulk_insert(&mut self, entries: &[KVEntry]) {
        for entry in entries {
            self.write_buffer.insert(entry.hash.clone(), entry.clone());
            self.entry_count += 1;

            if self.write_buffer.len() >= WRITE_BUFFER_THRESHOLD {
                if let Err(e) = self.flush_no_snapshot() {
                    tracing::warn!("Flush failed during bulk_insert: {}", e);
                }
            }
        }
    }

    fn flush_no_snapshot(&mut self) -> EngineResult<()> {
        if self.write_buffer.is_empty() { return Ok(()); }

        let hash_length = self.hash_algo.hash_length();
        let buffer_entries: Vec<KVEntry> = self.write_buffer.values().cloned().collect();
        let mut by_bucket: HashMap<usize, Vec<KVEntry>> = HashMap::new();
        for entry in buffer_entries {
            let bucket = self.nvt.bucket_for_value(&entry.hash);
            by_bucket.entry(bucket).or_default().push(entry);
        }

        for (bucket_index, new_entries) in by_bucket {
            let offset = self.kv_block_offset + bucket_page_offset(bucket_index, hash_length);
            let psize = page_size(hash_length);

            let mut page_data = vec![0u8; psize];
            self.db_file.seek(SeekFrom::Start(offset))?;
            self.db_file.read_exact(&mut page_data)?;

            let mut existing = match deserialize_page(&page_data, hash_length) {
                Ok(entries) => entries,
                Err(_) => {
                    let empty_page = vec![0u8; psize];
                    self.db_file.seek(SeekFrom::Start(offset))?;
                    self.db_file.write_all(&empty_page)?;
                    self.needs_rebuild = true;
                    Vec::new()
                }
            };

            for entry in new_entries {
                upsert_in_page(&mut existing, entry);
            }

            let serialized = serialize_page(&existing, hash_length);
            self.db_file.seek(SeekFrom::Start(offset))?;
            self.db_file.write_all(&serialized)?;
        }

        self.db_file.sync_data()?;
        self.write_buffer.clear();
        Ok(())
    }

    fn entry_exists_on_disk(&self, hash: &[u8]) -> bool {
        let current = self.snapshot.load();
        current.get_raw(hash).is_some()
    }

    /// Flush the write buffer to KV bucket pages.
    pub fn flush(&mut self) -> EngineResult<()> {
        if self.write_buffer.is_empty() { return Ok(()); }
        let timer_start = std::time::Instant::now();
        tracing::debug!(
            write_buffer_len = self.write_buffer.len(),
            bucket_count = self.bucket_count,
            stage = self.stage,
            kv_block_offset = self.kv_block_offset,
            kv_block_length = self.kv_block_length,
            "flush: starting"
        );

        let hash_length = self.hash_algo.hash_length();
        let mut overflow_entries: Vec<KVEntry> = Vec::new();

        let buffer_entries: Vec<KVEntry> = self.write_buffer.values().cloned().collect();
        let mut by_bucket: HashMap<usize, Vec<KVEntry>> = HashMap::new();
        for entry in buffer_entries {
            let bucket = self.nvt.bucket_for_value(&entry.hash);
            by_bucket.entry(bucket).or_default().push(entry);
        }

        let modified_buckets: Vec<usize> = by_bucket.keys().cloned().collect();

        for (bucket_index, new_entries) in by_bucket {
            let offset = self.kv_block_offset + bucket_page_offset(bucket_index, hash_length);
            let psize = page_size(hash_length);

            let mut page_data = vec![0u8; psize];
            self.db_file.seek(SeekFrom::Start(offset))?;
            self.db_file.read_exact(&mut page_data)?;

            let mut existing = match deserialize_page(&page_data, hash_length) {
                Ok(entries) => entries,
                Err(e) => {
                    tracing::warn!("Corrupt KV page at bucket {}: {}. Resetting.", bucket_index, e);
                    let empty_page = vec![0u8; psize];
                    self.db_file.seek(SeekFrom::Start(offset))?;
                    self.db_file.write_all(&empty_page)?;
                    self.needs_rebuild = true;
                    Vec::new()
                }
            };

            for entry in new_entries {
                if !upsert_in_page(&mut existing, entry.clone()) {
                    overflow_entries.push(entry);
                }
            }

            let serialized = serialize_page(&existing, hash_length);
            self.db_file.seek(SeekFrom::Start(offset))?;
            self.db_file.write_all(&serialized)?;
        }

        self.db_file.sync_data()?;
        self.write_buffer.clear();

        tracing::debug!(
            overflow_count = overflow_entries.len(),
            modified_buckets = modified_buckets.len(),
            "flush: pages written"
        );

        if !overflow_entries.is_empty() {
            // Publish snapshot BEFORE resize so iter_all sees flushed entries
            self.publish_snapshot_incremental(&modified_buckets);
            let old_stage = self.stage;
            self.resize_to_next_stage()?;
            if self.stage > old_stage {
                // Resize succeeded — re-insert overflow and flush again
                for entry in overflow_entries {
                    self.write_buffer.insert(entry.hash.clone(), entry);
                }
                return self.flush();
            } else {
                // Resize blocked (block too small) — keep overflow in write buffer.
                // They're queryable via snapshot and will be persisted in the hot tail.
                for entry in overflow_entries {
                    self.write_buffer.insert(entry.hash.clone(), entry);
                }
                // Write overflow entries to hot tail for crash recovery
                if self.hot_tail_enabled {
                    let hash_length = self.hash_algo.hash_length();
                    let all_hot: Vec<KVEntry> = self.write_buffer.values().cloned().collect();
                    tracing::debug!(
                        overflow_count = all_hot.len(),
                        hot_tail_offset = self.hot_tail_offset,
                        "flush: writing overflow entries to hot tail (resize blocked)"
                    );
                    let end = hot_tail::write_hot_tail(&mut self.db_file, self.hot_tail_offset, &all_hot, hash_length)?;
                    self.db_file.set_len(end)?; // Truncate stale trailing data
                    self.db_file.sync_data()?;
                }
                self.publish_snapshot_incremental(&modified_buckets);
                self.publish_buffer_only();
                let elapsed = timer_start.elapsed().as_secs_f64();
                metrics::histogram!(crate::metrics::definitions::KV_FLUSH_DURATION).record(elapsed);
                return Ok(());
            }
        }

        // All entries flushed to pages — clear hot tail
        tracing::debug!(
            hot_tail_offset = self.hot_tail_offset,
            "flush: all entries fit in pages, clearing hot tail"
        );
        self.flush_hot_buffer()?;
        if self.transaction_depth == 0 && self.hot_tail_enabled {
            let hash_length = self.hash_algo.hash_length();
            let _ = hot_tail::write_hot_tail(&mut self.db_file, self.hot_tail_offset, &[], hash_length);
        }

        self.publish_snapshot_incremental(&modified_buckets);

        let elapsed = timer_start.elapsed().as_secs_f64();
        metrics::histogram!(crate::metrics::definitions::KV_FLUSH_DURATION).record(elapsed);

        Ok(())
    }

    /// Resize the KV store to the next stage.
    /// Currently creates a temp sidecar for migration. In the future, this will
    /// do in-place expansion via background WAL relocation (Task 6).
    pub fn resize_to_next_stage(&mut self) -> EngineResult<()> {
        let new_stage = (self.stage + 1).min(KV_STAGE_SIZES.len() - 1);
        if new_stage == self.stage {
            return Err(EngineError::IoError(std::io::Error::other(
                "KV store at maximum stage — cannot resize further",
            )));
        }

        let hash_length = self.hash_algo.hash_length();
        let psize = page_size(hash_length);
        let (_block_size, new_bucket_count) = stage_params(new_stage, psize);

        // Check that new pages fit within the KV block
        let new_pages_size = (new_bucket_count as u64) * (psize as u64);
        if new_pages_size > self.kv_block_length {
            // Can't resize in-place — the KV block is too small.
            // Overflow entries will remain in the write buffer (still queryable
            // via snapshot). StorageEngine must expand the block before resize.
            tracing::warn!(
                "KV resize blocked: stage {} needs {}B but block is {}B. Overflow entries stay in write buffer.",
                new_stage, new_pages_size, self.kv_block_length,
            );
            return Ok(());
        }

        // Read all non-deleted entries
        let all_entries = self.iter_all()?;

        // Zero-fill new pages
        let empty_page = vec![0u8; psize];
        for bucket in 0..new_bucket_count {
            let offset = self.kv_block_offset + bucket_page_offset(bucket, hash_length);
            self.db_file.seek(SeekFrom::Start(offset))?;
            self.db_file.write_all(&empty_page)?;
        }
        self.db_file.sync_data()?;

        // Update internal state
        self.stage = new_stage;
        self.bucket_count = new_bucket_count;
        self.nvt = NormalizedVectorTable::new(Box::new(HashConverter), new_bucket_count);
        self.entry_count = 0;

        // Re-insert all entries
        self.bulk_insert(&all_entries);
        self.flush_no_snapshot()?;

        self.entry_count = all_entries.len();
        self.publish_full_snapshot_with_new_nvt();

        Ok(())
    }

    pub fn contains(&mut self, hash: &[u8]) -> bool {
        self.get(hash).is_some()
    }

    pub fn mark_deleted(&mut self, hash: &[u8]) {
        if let Some(mut entry) = self.get(hash) {
            entry.type_flags |= KV_FLAG_DELETED;
            self.write_buffer.insert(hash.to_vec(), entry);
            self.entry_count = self.entry_count.saturating_sub(1);
            self.publish_buffer_only();
        }
    }

    pub fn mark_deleted_batch(&mut self, hashes: &[Vec<u8>]) {
        for hash in hashes {
            if let Some(mut entry) = self.get(hash) {
                entry.type_flags |= KV_FLAG_DELETED;
                self.write_buffer.insert(hash.clone(), entry);
                self.entry_count = self.entry_count.saturating_sub(1);
            }
            if self.write_buffer.len() >= WRITE_BUFFER_THRESHOLD {
                if let Err(e) = self.flush() {
                    tracing::warn!("Flush failed during mark_deleted_batch: {}", e);
                }
            }
        }
        self.publish_buffer_only();
    }

    pub fn iter_all(&mut self) -> EngineResult<Vec<KVEntry>> {
        let hash_length = self.hash_algo.hash_length();
        let psize = page_size(hash_length);
        let mut all: HashMap<Vec<u8>, KVEntry> = HashMap::new();

        for bucket in 0..self.bucket_count {
            let offset = self.kv_block_offset + bucket_page_offset(bucket, hash_length);
            let mut page_data = vec![0u8; psize];
            self.db_file.seek(SeekFrom::Start(offset))?;
            if self.db_file.read_exact(&mut page_data).is_ok() {
                if let Ok(entries) = deserialize_page(&page_data, hash_length) {
                    for entry in entries {
                        all.insert(entry.hash.clone(), entry);
                    }
                }
            }
        }

        for (hash, entry) in &self.write_buffer {
            all.insert(hash.clone(), entry.clone());
        }

        Ok(all.into_values().filter(|e| !e.is_deleted()).collect())
    }

    pub fn len(&self) -> usize { self.entry_count }
    pub fn is_empty(&self) -> bool { self.entry_count == 0 }
    pub fn write_buffer_len(&self) -> usize { self.write_buffer.len() }

    /// Look up an entry in the write buffer only (no disk read).
    pub fn get_buffered(&self, hash: &[u8]) -> Option<&KVEntry> {
        self.write_buffer.get(hash)
    }

    /// Clear the write buffer without flushing. Used before dropping a KV
    /// store that is being replaced (e.g., after rebuild_kv) to prevent
    /// the Drop impl from overwriting newly-rebuilt pages with stale data.
    pub fn clear_write_buffer(&mut self) { self.write_buffer.clear(); }

    /// Insert an entry into the write buffer without triggering auto-flush
    /// or hot buffer journaling. Used by rebuild_kv to accumulate all entries
    /// before a single flush, preventing page clobbering across flush cycles.
    pub fn buffer_only(&mut self, entry: KVEntry) {
        let is_new = !self.write_buffer.contains_key(&entry.hash);
        self.write_buffer.insert(entry.hash.clone(), entry);
        if is_new { self.entry_count += 1; }
    }

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

    pub fn stage(&self) -> usize { self.stage }
    pub fn bucket_count(&self) -> usize { self.bucket_count }
    pub fn hash_algo(&self) -> HashAlgorithm { self.hash_algo }
    pub fn hot_tail_offset(&self) -> u64 { self.hot_tail_offset }

    /// Update the hot tail offset (called by StorageEngine after a WAL append).
    pub fn set_hot_tail_offset(&mut self, offset: u64) {
        self.hot_tail_offset = offset;
    }

    // ========================================================================
    // Snapshot publishing
    // ========================================================================

    fn read_all_pages(&mut self) -> Arc<Vec<Vec<u8>>> {
        let hash_length = self.hash_algo.hash_length();
        let psize = page_size(hash_length);
        let mut pages = Vec::with_capacity(self.bucket_count);
        for bucket in 0..self.bucket_count {
            let offset = self.kv_block_offset + bucket_page_offset(bucket, hash_length);
            let mut page_data = vec![0u8; psize];
            if self.db_file.seek(SeekFrom::Start(offset)).is_ok() {
                if self.db_file.read_exact(&mut page_data).is_ok() {
                    pages.push(page_data);
                    continue;
                }
            }
            pages.push(vec![0u8; psize]);
        }
        Arc::new(pages)
    }

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

    fn publish_snapshot_incremental(&mut self, modified_buckets: &[usize]) {
        if self.shared_nvt.bucket_count() != self.nvt.bucket_count() {
            self.shared_nvt = Arc::new(self.nvt.clone());
        }

        let current = self.snapshot.load();
        let old_pages = current.pages();
        let mut new_pages = (**old_pages).clone();

        let hash_length = self.hash_algo.hash_length();
        let psize = page_size(hash_length);
        for &bucket in modified_buckets {
            if bucket < new_pages.len() {
                let offset = self.kv_block_offset + bucket_page_offset(bucket, hash_length);
                let mut page_data = vec![0u8; psize];
                if self.db_file.seek(SeekFrom::Start(offset)).is_ok() {
                    let _ = self.db_file.read_exact(&mut page_data);
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

    fn publish_full_snapshot_with_new_nvt(&mut self) {
        self.shared_nvt = Arc::new(self.nvt.clone());
        self.publish_full_snapshot();
    }

    pub fn snapshot_handle(&self) -> &Arc<ArcSwap<ReadSnapshot>> {
        &self.snapshot
    }

    // ========================================================================
    // Hot tail (replaces hot file)
    // ========================================================================

    /// Flush the hot buffer to the hot tail at the end of the database file.
    pub fn flush_hot_buffer(&mut self) -> EngineResult<()> {
        if self.hot_buffer.is_empty() || !self.hot_tail_enabled {
            return Ok(());
        }

        let hash_length = self.hash_algo.hash_length();

        // Collect ALL entries that need to be in the hot tail:
        // everything in the write buffer (these haven't been flushed to pages yet)
        let all_hot: Vec<KVEntry> = self.write_buffer.values().cloned().collect();

        let end = hot_tail::write_hot_tail(&mut self.db_file, self.hot_tail_offset, &all_hot, hash_length)?;
        self.db_file.set_len(end)?; // Truncate stale trailing data
        self.db_file.sync_data()?;
        self.hot_buffer.clear();

        Ok(())
    }

    /// Number of entries in the hot buffer.
    pub fn hot_buffer_len(&self) -> usize {
        self.hot_buffer.len()
    }
}

impl Drop for DiskKVStore {
    fn drop(&mut self) {
        if !self.write_buffer.is_empty() {
            if let Err(e) = self.flush() {
                tracing::error!("DiskKVStore: failed to flush on drop: {}", e);
            }
        }
    }
}
