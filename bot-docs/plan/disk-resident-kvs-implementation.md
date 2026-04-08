# Disk-Resident KV Store Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the in-memory sorted Vec KV store with a disk-resident bucket-page KV store indexed by the NVT, fixing the O(N) Vec::insert degradation at scale while bounding memory usage.

**Architecture:** NVT stays in memory (scales with stage table). KV entries stored on disk in bucket pages (bucket N at fixed offset). Write buffer (bounded HashMap) absorbs recent writes. Hot cache (LRU) caches recent reads. Lookup: buffer → cache → NVT bucket → disk page → scan entries. The KVStore public API stays the same — StorageEngine callers don't change.

**Tech Stack:** Rust, std::fs (seek + read/write for bucket pages), std::collections::HashMap (write buffer + cache)

**Spec:** `bot-docs/plan/disk-resident-kvs.md`

---

### Task 1: KV bucket page format + read/write primitives

The foundation: define how a bucket page is stored on disk and implement read/write operations for individual pages.

**Files:**
- Create: `aeordb-lib/src/engine/kv_pages.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`
- Create: `aeordb-lib/spec/engine/kv_pages_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Create kv_pages.rs with page format**

KV entry on disk: `hash(N bytes) + type_flags(1 byte) + offset(8 bytes)`.
Page: `entry_count(u16) + entries[]`.

```rust
use crate::engine::kv_store::KVEntry;
use crate::engine::errors::{EngineError, EngineResult};

/// Stage table for KV block growth.
pub const KV_STAGES: &[(u64, usize)] = &[
    //  (block_size, nvt_buckets)
    (64 * 1024,          1_024),   // Stage 0: 64KB
    (256 * 1024,         4_096),   // Stage 1: 256KB
    (1024 * 1024,        8_192),   // Stage 2: 1MB
    (4 * 1024 * 1024,   16_384),   // Stage 3: 4MB
    (16 * 1024 * 1024,  32_768),   // Stage 4: 16MB
    (64 * 1024 * 1024,  65_536),   // Stage 5: 64MB
    (256 * 1024 * 1024, 65_536),   // Stage 6: 256MB
    (1024 * 1024 * 1024, 131_072), // Stage 7: 1GB
];

/// Maximum entries per bucket page.
pub const MAX_ENTRIES_PER_PAGE: usize = 32;

/// Compute the byte size of one bucket page for a given hash length.
pub fn page_size(hash_length: usize) -> usize {
    // entry_count(2) + MAX_ENTRIES_PER_PAGE * (hash + type_flags + offset)
    2 + MAX_ENTRIES_PER_PAGE * (hash_length + 1 + 8)
}

/// Compute the offset of bucket N's page within the KV block.
pub fn bucket_page_offset(bucket_index: usize, hash_length: usize) -> u64 {
    (bucket_index * page_size(hash_length)) as u64
}

/// Serialize a list of KV entries into a bucket page.
pub fn serialize_page(entries: &[KVEntry], hash_length: usize) -> Vec<u8> {
    let psize = page_size(hash_length);
    let mut buffer = vec![0u8; psize];
    let count = entries.len().min(MAX_ENTRIES_PER_PAGE);

    buffer[0..2].copy_from_slice(&(count as u16).to_le_bytes());

    let entry_size = hash_length + 1 + 8;
    for (i, entry) in entries.iter().take(count).enumerate() {
        let offset = 2 + i * entry_size;
        let hash_len = entry.hash.len().min(hash_length);
        buffer[offset..offset + hash_len].copy_from_slice(&entry.hash[..hash_len]);
        buffer[offset + hash_length] = entry.type_flags;
        buffer[offset + hash_length + 1..offset + hash_length + 9]
            .copy_from_slice(&entry.offset.to_le_bytes());
    }

    buffer
}

/// Deserialize a bucket page into KV entries.
pub fn deserialize_page(data: &[u8], hash_length: usize) -> EngineResult<Vec<KVEntry>> {
    if data.len() < 2 {
        return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: "Page data too short".to_string(),
        });
    }

    let count = u16::from_le_bytes([data[0], data[1]]) as usize;
    let entry_size = hash_length + 1 + 8;
    let mut entries = Vec::with_capacity(count);

    for i in 0..count {
        let offset = 2 + i * entry_size;
        if offset + entry_size > data.len() {
            break;
        }
        let hash = data[offset..offset + hash_length].to_vec();
        let type_flags = data[offset + hash_length];
        let file_offset = u64::from_le_bytes(
            data[offset + hash_length + 1..offset + hash_length + 9].try_into().unwrap()
        );
        entries.push(KVEntry { type_flags, hash, offset: file_offset });
    }

    Ok(entries)
}

/// Find an entry by hash within a deserialized page.
pub fn find_in_page(entries: &[KVEntry], hash: &[u8]) -> Option<&KVEntry> {
    entries.iter().find(|e| e.hash == hash && !e.is_deleted())
}

/// Insert or update an entry in a page's entry list. Returns true if page has space.
pub fn upsert_in_page(entries: &mut Vec<KVEntry>, entry: KVEntry) -> bool {
    if let Some(existing) = entries.iter_mut().find(|e| e.hash == entry.hash) {
        *existing = entry;
        true
    } else if entries.len() < MAX_ENTRIES_PER_PAGE {
        entries.push(entry);
        true
    } else {
        false // page full
    }
}

/// Determine the appropriate stage for a given entry count.
pub fn stage_for_count(entry_count: usize, hash_length: usize) -> usize {
    let per_page = MAX_ENTRIES_PER_PAGE;
    for (stage, (block_size, buckets)) in KV_STAGES.iter().enumerate() {
        let capacity = buckets * per_page;
        if entry_count < capacity {
            return stage;
        }
    }
    KV_STAGES.len() - 1
}
```

- [ ] **Step 2: Write tests for page primitives**

Tests:
1. `test_serialize_deserialize_empty_page` — roundtrip empty page
2. `test_serialize_deserialize_with_entries` — roundtrip page with entries
3. `test_find_in_page` — find by hash
4. `test_find_in_page_missing` — not found
5. `test_find_in_page_skips_deleted` — deleted entries not found
6. `test_upsert_insert` — add new entry
7. `test_upsert_update` — update existing
8. `test_upsert_full` — page at capacity returns false
9. `test_page_size_calculation` — correct for various hash lengths
10. `test_bucket_page_offset` — sequential offsets
11. `test_stage_for_count` — correct stage at various sizes
12. `test_stage_table_monotonic` — stages increase in size and buckets

- [ ] **Step 3: Register module, build, test**

- [ ] **Step 4: Commit**

---

### Task 2: DiskKVStore — the new KV store implementation

Replace the sorted Vec with a disk-resident bucket-page store. Same public API as KVStore.

**Files:**
- Create: `aeordb-lib/src/engine/disk_kv_store.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`
- Create: `aeordb-lib/spec/engine/disk_kv_store_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Create DiskKVStore**

```rust
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::kv_store::KVEntry;
use crate::engine::kv_pages::*;
use crate::engine::nvt::NormalizedVectorTable;
use crate::engine::scalar_converter::HashConverter;

const WRITE_BUFFER_THRESHOLD: usize = 1000;
const HOT_CACHE_MAX: usize = 10_000;

pub struct DiskKVStore {
    /// NVT for bucket lookup (in memory, small)
    nvt: NormalizedVectorTable,
    /// Write buffer for recent inserts (not yet on disk)
    write_buffer: HashMap<Vec<u8>, KVEntry>,
    /// Hot cache for recently read entries from disk
    hot_cache: HashMap<Vec<u8>, KVEntry>,
    /// LRU order tracking for hot cache eviction
    cache_order: Vec<Vec<u8>>,
    /// File handle for the KV block on disk
    kv_file: File,
    /// Path to the KV block file
    kv_path: PathBuf,
    /// Current stage in the stage table
    stage: usize,
    /// Hash algorithm (for hash length)
    hash_algo: HashAlgorithm,
    /// Total entry count (for stage calculations)
    entry_count: usize,
}
```

Actually — the KV block is part of the main .aeordb file, not a separate file. It's at `kv_block_offset` in the file header. The DiskKVStore needs access to the same file handle as the AppendWriter.

This creates a complexity: the AppendWriter owns the file handle. The DiskKVStore needs to read/write the KV block section of the same file.

**Simpler approach for Phase 1:** Use a SEPARATE file for the KV block (e.g., `data.aeordb.kv`). This avoids file handle sharing issues. Later we can embed it in the main file.

```rust
pub struct DiskKVStore {
    nvt: NormalizedVectorTable,
    write_buffer: HashMap<Vec<u8>, KVEntry>,
    hot_cache: HashMap<Vec<u8>, KVEntry>,
    cache_order: Vec<Vec<u8>>,
    kv_file: File,
    kv_path: PathBuf,
    stage: usize,
    hash_algo: HashAlgorithm,
    entry_count: usize,
    bucket_count: usize,
}

impl DiskKVStore {
    /// Create a new disk KV store at the given path.
    pub fn create(path: &Path, hash_algo: HashAlgorithm) -> EngineResult<Self> {
        let stage = 0;
        let (block_size, bucket_count) = KV_STAGES[stage];
        let hash_length = hash_algo.hash_length();

        // Create file with initial size
        let mut file = OpenOptions::new()
            .read(true).write(true).create_new(true)
            .open(path)?;

        // Write empty pages
        let empty_page = vec![0u8; page_size(hash_length)];
        for _ in 0..bucket_count {
            file.write_all(&empty_page)?;
        }
        file.sync_all()?;

        let nvt = NormalizedVectorTable::new(
            Box::new(HashConverter), bucket_count,
        );

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

    /// Open an existing disk KV store.
    pub fn open(path: &Path, hash_algo: HashAlgorithm) -> EngineResult<Self> {
        let file = OpenOptions::new()
            .read(true).write(true)
            .open(path)?;

        let file_size = file.metadata()?.len();
        let hash_length = hash_algo.hash_length();
        let psize = page_size(hash_length) as u64;

        // Determine stage from file size
        let mut stage = 0;
        let mut bucket_count = KV_STAGES[0].1;
        for (s, (block_size, buckets)) in KV_STAGES.iter().enumerate() {
            if file_size >= *buckets as u64 * psize {
                stage = s;
                bucket_count = *buckets;
            }
        }

        // Rebuild NVT from disk pages
        let mut nvt = NormalizedVectorTable::new(
            Box::new(HashConverter), bucket_count,
        );
        let mut entry_count = 0;
        // ... read each page, count entries, update NVT buckets ...

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
    pub fn get(&mut self, hash: &[u8]) -> Option<KVEntry> {
        // 1. Check write buffer
        if let Some(entry) = self.write_buffer.get(hash) {
            if !entry.is_deleted() {
                return Some(entry.clone());
            } else {
                return None;
            }
        }

        // 2. Check hot cache
        if let Some(entry) = self.hot_cache.get(hash) {
            return Some(entry.clone());
        }

        // 3. Read from disk via NVT
        let bucket_index = self.nvt.bucket_for_value(hash);
        let hash_length = self.hash_algo.hash_length();
        let offset = bucket_page_offset(bucket_index, hash_length);

        let mut page_data = vec![0u8; page_size(hash_length)];
        if self.kv_file.seek(SeekFrom::Start(offset)).is_err() {
            return None;
        }
        if self.kv_file.read_exact(&mut page_data).is_err() {
            return None;
        }

        let entries = deserialize_page(&page_data, hash_length).ok()?;
        let found = find_in_page(&entries, hash)?.clone();

        // Cache it
        self.cache_put(hash, &found);

        Some(found)
    }

    /// Insert or update an entry.
    pub fn insert(&mut self, entry: KVEntry) {
        self.hot_cache.remove(&entry.hash);
        self.write_buffer.insert(entry.hash.clone(), entry);
        self.entry_count += 1;

        if self.write_buffer.len() >= WRITE_BUFFER_THRESHOLD {
            let _ = self.flush();
        }
    }

    /// Flush the write buffer to disk.
    pub fn flush(&mut self) -> EngineResult<()> {
        if self.write_buffer.is_empty() {
            return Ok(());
        }

        let hash_length = self.hash_algo.hash_length();

        // Group buffered entries by bucket
        let mut by_bucket: HashMap<usize, Vec<KVEntry>> = HashMap::new();
        for (_, entry) in self.write_buffer.drain() {
            let bucket = self.nvt.bucket_for_value(&entry.hash);
            by_bucket.entry(bucket).or_default().push(entry);
        }

        // For each affected bucket, read page, merge, write back
        for (bucket_index, new_entries) in by_bucket {
            let offset = bucket_page_offset(bucket_index, hash_length);
            let psize = page_size(hash_length);

            // Read existing page
            let mut page_data = vec![0u8; psize];
            self.kv_file.seek(SeekFrom::Start(offset))?;
            self.kv_file.read_exact(&mut page_data)?;

            let mut existing = deserialize_page(&page_data, hash_length)?;

            // Merge new entries
            for entry in new_entries {
                if !upsert_in_page(&mut existing, entry) {
                    // Page full — need to resize (stage up)
                    // For now, return error. Task 5 handles resize.
                    return Err(EngineError::IoError(
                        std::io::Error::other("KV bucket page overflow — resize needed")
                    ));
                }
            }

            // Write page back
            let serialized = serialize_page(&existing, hash_length);
            self.kv_file.seek(SeekFrom::Start(offset))?;
            self.kv_file.write_all(&serialized)?;
        }

        self.kv_file.sync_data()?;
        Ok(())
    }

    /// Check if an entry exists.
    pub fn contains(&mut self, hash: &[u8]) -> bool {
        self.get(hash).is_some()
    }

    /// Mark an entry as deleted.
    pub fn mark_deleted(&mut self, hash: &[u8]) {
        if let Some(mut entry) = self.get(hash) {
            entry.type_flags |= crate::engine::kv_store::KV_FLAG_DELETED;
            self.write_buffer.insert(hash.to_vec(), entry);
            self.hot_cache.remove(hash);
        }
    }

    /// Iterate all entries (reads all pages from disk).
    pub fn iter_all(&mut self) -> EngineResult<Vec<KVEntry>> {
        let hash_length = self.hash_algo.hash_length();
        let psize = page_size(hash_length);
        let mut all = Vec::new();

        // Read all pages
        for bucket in 0..self.bucket_count {
            let offset = bucket_page_offset(bucket, hash_length);
            let mut page_data = vec![0u8; psize];
            self.kv_file.seek(SeekFrom::Start(offset))?;
            if self.kv_file.read_exact(&mut page_data).is_ok() {
                if let Ok(entries) = deserialize_page(&page_data, hash_length) {
                    all.extend(entries);
                }
            }
        }

        // Merge write buffer (buffer takes priority)
        for (hash, entry) in &self.write_buffer {
            if let Some(existing) = all.iter_mut().find(|e| e.hash == *hash) {
                *existing = entry.clone();
            } else {
                all.push(entry.clone());
            }
        }

        Ok(all)
    }

    /// Total entry count.
    pub fn len(&self) -> usize {
        self.entry_count
    }

    pub fn is_empty(&self) -> bool {
        self.entry_count == 0
    }

    fn cache_put(&mut self, hash: &[u8], entry: &KVEntry) {
        if self.hot_cache.len() >= HOT_CACHE_MAX {
            // Evict oldest
            if let Some(old_hash) = self.cache_order.first().cloned() {
                self.hot_cache.remove(&old_hash);
                self.cache_order.remove(0);
            }
        }
        self.hot_cache.insert(hash.to_vec(), entry.clone());
        self.cache_order.push(hash.to_vec());
    }
}
```

- [ ] **Step 2: Write tests**

Tests using a temp file:
1. `test_create_and_open` — create, close, reopen
2. `test_insert_and_get` — insert entry, get it back
3. `test_insert_multiple` — insert 100, all findable
4. `test_get_missing` — not found → None
5. `test_contains` — true/false
6. `test_mark_deleted` — deleted entry not found
7. `test_flush_writes_to_disk` — insert, flush, reopen, entry persists
8. `test_write_buffer_auto_flush` — insert > threshold, auto-flushed
9. `test_hot_cache` — second get is cached (no disk read)
10. `test_iter_all` — all entries returned
11. `test_iter_all_with_buffer` — unflushed entries included
12. `test_upsert_existing` — update same hash
13. `test_large_dataset` — 5000 entries, all findable
14. `test_entry_count` — len() tracks correctly

- [ ] **Step 3: Register, build, test, commit**

---

### Task 3: Wire DiskKVStore into StorageEngine

Replace the Vec-based KVStore with DiskKVStore in StorageEngine. The StorageEngine public API doesn't change.

**Files:**
- Modify: `aeordb-lib/src/engine/storage_engine.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`

- [ ] **Step 1: Replace KVStore with DiskKVStore in StorageEngine**

The `kv_manager: RwLock<KVResizeManager>` field wraps a KVStore. Replace the internals:

Option A: Replace KVResizeManager entirely with DiskKVStore.
Option B: Make KVResizeManager wrap DiskKVStore.

**Go with Option A** — DiskKVStore handles its own resize (stage table). KVResizeManager is no longer needed.

Change StorageEngine:
```rust
pub struct StorageEngine {
    writer: RwLock<AppendWriter>,
    kv_store: RwLock<DiskKVStore>,  // was kv_manager: RwLock<KVResizeManager>
    void_manager: RwLock<VoidManager>,
    hash_algo: HashAlgorithm,
}
```

Update ALL methods that access `kv_manager` to use `kv_store` instead. The DiskKVStore has the same core methods (`get`, `insert`, `contains`) but with slightly different signatures (e.g., `get` returns `Option<KVEntry>` not `Option<&KVEntry>`).

Key changes:
- `create()`: create DiskKVStore alongside the .aeordb file (at `path.kv`)
- `open()`: open DiskKVStore, then scan entries to populate it (same as today, but inserting into DiskKVStore)
- `open_for_import()`: same
- `get_entry()`: kv_store.get(hash) → if found, read entry from file at offset
- `store_entry()`: append to file, kv_store.insert(KVEntry)
- `has_entry()`: kv_store.contains(hash)
- `mark_entry_deleted()`: kv_store.mark_deleted(hash)
- `entries_by_type()`: kv_store.iter_all() → filter by type
- `flush_batch()`: append entries, kv_store.insert for each
- `stats()`: kv_store.len() for counts

- [ ] **Step 2: Update create_engine_for_storage and create_temp_engine_for_tests**

The KV file path is derived from the engine path: `engine_path.replace(".aeordb", ".aeordb.kv")` or `format!("{}.kv", engine_path)`.

- [ ] **Step 3: Run ALL existing tests**

This is the critical step. Every test goes through StorageEngine. The interface is the same, but the internals are completely different. ALL 2,037 tests must pass.

- [ ] **Step 4: Commit**

---

### Task 4: Startup without full entry scan

Currently `StorageEngine::open` scans ALL entries to rebuild the KV store. With DiskKVStore, the KV data is already on disk — no scan needed.

**Files:**
- Modify: `aeordb-lib/src/engine/storage_engine.rs`

- [ ] **Step 1: Change open to read DiskKVStore from disk**

```rust
pub fn open(path: &str) -> EngineResult<Self> {
    let writer = AppendWriter::open(Path::new(path))?;
    let hash_algo = writer.file_header().hash_algo;

    // Open existing disk KV store (no entry scan)
    let kv_path = format!("{}.kv", path);
    let kv_store = if Path::new(&kv_path).exists() {
        DiskKVStore::open(Path::new(&kv_path), hash_algo)?
    } else {
        // First time (or migration): create KV store from entry scan
        let mut kv = DiskKVStore::create(Path::new(&kv_path), hash_algo)?;
        // ... scan entries and populate kv (existing code) ...
        kv
    };

    // ... rest of open ...
}
```

- [ ] **Step 2: Add cross-restart test**

Store 1000 files, close, reopen WITHOUT scan, verify all files accessible.

- [ ] **Step 3: Commit**

---

### Task 5: KV resize (stage up on overflow)

When a bucket page overflows during flush, grow the KV store to the next stage.

**Files:**
- Modify: `aeordb-lib/src/engine/disk_kv_store.rs`

- [ ] **Step 1: Implement resize**

```rust
pub fn resize_to_stage(&mut self, new_stage: usize) -> EngineResult<()> {
    let (new_block_size, new_bucket_count) = KV_STAGES[new_stage];
    let hash_length = self.hash_algo.hash_length();

    // Create new KV file
    let new_path = self.kv_path.with_extension("kv.new");
    let mut new_store = DiskKVStore::create(&new_path, self.hash_algo)?;

    // Copy all existing entries to new store
    let all_entries = self.iter_all()?;
    for entry in all_entries {
        new_store.insert(entry);
    }
    new_store.flush()?;

    // Swap files
    let old_path = self.kv_path.clone();
    std::fs::rename(&new_path, &old_path)?;

    // Reopen
    *self = DiskKVStore::open(&old_path, self.hash_algo)?;
    self.stage = new_stage;

    Ok(())
}
```

Update `flush()` to catch the overflow error and trigger resize:

```rust
// In flush(), replace the overflow error with:
if !upsert_in_page(&mut existing, entry.clone()) {
    // Page full — stage up and retry
    drop(by_bucket); // release borrows
    let new_stage = (self.stage + 1).min(KV_STAGES.len() - 1);
    self.resize_to_stage(new_stage)?;
    // Re-insert the entry (now there's room)
    self.insert(entry);
    return self.flush();
}
```

- [ ] **Step 2: Write tests**

1. `test_resize_on_overflow` — fill a stage-0 store, trigger resize, entries preserved
2. `test_resize_preserves_all_entries` — 2000 entries, resize, all findable
3. `test_stage_increases` — after resize, stage is higher

- [ ] **Step 3: Commit**

---

### Task 6: Benchmark

- [ ] **Step 1: Build release binary**

```bash
cargo build --release
```

- [ ] **Step 2: Run stress test to 250K**

Compare against the previous benchmark:
- Before (Vec KV): 72/s at 250K
- Expected (Disk KV): ~500-800/s at 250K (flat throughput)

- [ ] **Step 3: Record results and commit**
