# Single-File Database Refactor — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Consolidate the three database files (.aeordb WAL, .kv sidecar, hot file) into a single .aeordb file with KV block at head, WAL in middle, hot tail at end.

**Architecture:** The file header (256 bytes) is followed by the KV bucket pages (staged growth, pinned at head), then NVT data, then the WAL append area, then the hot tail at EOF. DiskKVStore reads/writes pages directly in the main file. The sidecar .kv file and hot file are eliminated. A write buffer (1,000 entries) and timer (250ms) control flush cadence. KV resize relocates WAL entries in background batches to make room at the head.

**Tech Stack:** Rust, std::fs (seek + read/write), std::io, tokio (timer), crc32fast, existing engine infrastructure

**Spec:** `docs/superpowers/specs/2026-04-28-single-file-database-design.md`
**Prior design:** `bot-docs/plan/disk-resident-kvs.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `aeordb-lib/src/engine/file_header.rs` | Modify | Add `hot_tail_offset`, `kv_block_stage`, `resize_target_stage` |
| `aeordb-lib/src/engine/hot_tail.rs` | Create | Hot tail format: magic, CRC, serialize/deserialize |
| `aeordb-lib/src/engine/kv_stages.rs` | Create | Stage table, growth calculation |
| `aeordb-lib/src/engine/disk_kv_store.rs` | Major rewrite | In-file KV pages, hot tail instead of hot file, timer flush |
| `aeordb-lib/src/engine/append_writer.rs` | Modify | Append area starts after NVT, writes before hot tail |
| `aeordb-lib/src/engine/storage_engine.rs` | Major rewrite | Single-file create/open, no sidecars, spawn timer |
| `aeordb-lib/src/engine/kv_resize.rs` | Create | Background batch relocation, KV expansion |
| `aeordb-lib/src/engine/mod.rs` | Modify | Register new modules |
| `aeordb-lib/Cargo.toml` | Modify | Add `crc32fast` dependency |
| `aeordb-lib/spec/engine/single_file_spec.rs` | Create | Integration tests |
| `aeordb-lib/spec/engine/hot_tail_spec.rs` | Create | Hot tail unit tests |
| `aeordb-lib/spec/engine/kv_resize_spec.rs` | Create | Resize tests |

---

### Task 1: Hot Tail Format

Independent data structure — no dependencies on other tasks. Testable in isolation.

**Files:**
- Create: `aeordb-lib/src/engine/hot_tail.rs`
- Create: `aeordb-lib/spec/engine/hot_tail_spec.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Add `crc32fast` dependency**

In `aeordb-lib/Cargo.toml`, add to `[dependencies]`:
```toml
crc32fast = "1"
```

- [ ] **Step 2: Create `hot_tail.rs`**

```rust
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::kv_store::KVEntry;
use std::io::{Read, Seek, SeekFrom, Write};

/// Magic bytes for the hot tail: 0xAE017DB100C (5 bytes).
/// "AE01 7DB 100C" — the database is running hot.
pub const HOT_TAIL_MAGIC: [u8; 5] = [0xAE, 0x01, 0x7D, 0xB1, 0x0C];

/// Total header size: magic(5) + entry_count(4) + crc32(4) = 13 bytes
pub const HOT_TAIL_HEADER_SIZE: usize = 13;

/// Serialize hot tail entries to bytes.
/// Format: magic(5) + entry_count(4) + crc32_of_count(4) + entries
pub fn serialize_hot_tail(entries: &[KVEntry], hash_length: usize) -> Vec<u8> {
    let entry_size = hash_length + 1 + 8; // hash + type_flags + offset
    let count = entries.len() as u32;
    let count_bytes = count.to_le_bytes();
    let crc = crc32fast::hash(&count_bytes);

    let mut buf = Vec::with_capacity(HOT_TAIL_HEADER_SIZE + entries.len() * entry_size);
    buf.extend_from_slice(&HOT_TAIL_MAGIC);
    buf.extend_from_slice(&count_bytes);
    buf.extend_from_slice(&crc.to_le_bytes());

    for entry in entries {
        let hash_bytes = &entry.hash;
        buf.extend_from_slice(&hash_bytes[..hash_length.min(hash_bytes.len())]);
        if hash_bytes.len() < hash_length {
            buf.resize(buf.len() + (hash_length - hash_bytes.len()), 0);
        }
        buf.push(entry.kv_type);
        buf.extend_from_slice(&entry.offset.to_le_bytes());
    }

    buf
}

/// Deserialize hot tail entries from bytes.
/// Returns None if magic or CRC doesn't match.
pub fn deserialize_hot_tail(data: &[u8], hash_length: usize) -> Option<Vec<KVEntry>> {
    if data.len() < HOT_TAIL_HEADER_SIZE {
        return None;
    }

    // Verify magic
    if data[..5] != HOT_TAIL_MAGIC {
        return None;
    }

    // Read and verify count
    let count_bytes: [u8; 4] = data[5..9].try_into().ok()?;
    let count = u32::from_le_bytes(count_bytes);
    let expected_crc = u32::from_le_bytes(data[9..13].try_into().ok()?);
    let actual_crc = crc32fast::hash(&count_bytes);
    if expected_crc != actual_crc {
        return None;
    }

    let entry_size = hash_length + 1 + 8;
    let expected_len = HOT_TAIL_HEADER_SIZE + (count as usize) * entry_size;
    if data.len() < expected_len {
        return None;
    }

    let mut entries = Vec::with_capacity(count as usize);
    let mut offset = HOT_TAIL_HEADER_SIZE;

    for _ in 0..count {
        let hash = data[offset..offset + hash_length].to_vec();
        offset += hash_length;
        let kv_type = data[offset];
        offset += 1;
        let entry_offset = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
        offset += 8;

        entries.push(KVEntry {
            hash,
            offset: entry_offset,
            kv_type,
        });
    }

    Some(entries)
}

/// Write hot tail to a file at the given offset.
pub fn write_hot_tail<W: Write + Seek>(
    writer: &mut W,
    offset: u64,
    entries: &[KVEntry],
    hash_length: usize,
) -> EngineResult<()> {
    let data = serialize_hot_tail(entries, hash_length);
    writer.seek(SeekFrom::Start(offset))?;
    writer.write_all(&data)?;
    // Truncate file to remove any stale data after the hot tail
    let end = offset + data.len() as u64;
    writer.seek(SeekFrom::Start(end))?;
    Ok(())
}

/// Read hot tail from a file at the given offset.
/// Returns None if the hot tail is corrupt or missing.
pub fn read_hot_tail<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    hash_length: usize,
) -> Option<Vec<KVEntry>> {
    if reader.seek(SeekFrom::Start(offset)).is_err() {
        return None;
    }

    // Read header first to get count
    let mut header = [0u8; HOT_TAIL_HEADER_SIZE];
    if reader.read_exact(&mut header).is_err() {
        return None;
    }

    if header[..5] != HOT_TAIL_MAGIC {
        return None;
    }

    let count_bytes: [u8; 4] = header[5..9].try_into().ok()?;
    let count = u32::from_le_bytes(count_bytes);
    let expected_crc = u32::from_le_bytes(header[9..13].try_into().ok()?);
    if crc32fast::hash(&count_bytes) != expected_crc {
        return None;
    }

    let entry_size = hash_length + 1 + 8;
    let entries_len = (count as usize) * entry_size;
    let mut entries_data = vec![0u8; entries_len];
    if reader.read_exact(&mut entries_data).is_err() {
        return None;
    }

    // Reassemble full data for deserialization
    let mut full = Vec::with_capacity(HOT_TAIL_HEADER_SIZE + entries_len);
    full.extend_from_slice(&header);
    full.extend_from_slice(&entries_data);
    deserialize_hot_tail(&full, hash_length)
}
```

- [ ] **Step 3: Register module**

Add `pub mod hot_tail;` to `aeordb-lib/src/engine/mod.rs`.

- [ ] **Step 4: Write tests**

Create `aeordb-lib/spec/engine/hot_tail_spec.rs`:

Tests:
1. `serialize_deserialize_roundtrip` — create 5 entries, serialize, deserialize, verify all fields match
2. `empty_hot_tail` — serialize/deserialize zero entries
3. `corrupt_magic_returns_none` — flip a magic byte, verify deserialize returns None
4. `corrupt_crc_returns_none` — change entry_count but not CRC, verify returns None
5. `truncated_data_returns_none` — serialize, truncate to half, verify returns None
6. `write_read_file_roundtrip` — write to a tempfile, read back, verify entries match

Register in `Cargo.toml`:
```toml
[[test]]
name = "hot_tail_spec"
path = "spec/engine/hot_tail_spec.rs"
```

- [ ] **Step 5: Verify**

Run: `cargo test --test hot_tail_spec 2>&1 | tail -10`

- [ ] **Step 6: Commit**

```bash
git add aeordb-lib/src/engine/hot_tail.rs aeordb-lib/src/engine/mod.rs aeordb-lib/Cargo.toml aeordb-lib/spec/engine/hot_tail_spec.rs
git commit -m "Add hot tail format: magic 0xAE017DB100C, CRC32 integrity, serialize/deserialize"
```

---

### Task 2: KV Stage Table

Extract the stage table into its own module with the tiered growth strategy.

**Files:**
- Create: `aeordb-lib/src/engine/kv_stages.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`
- Modify: `aeordb-lib/src/engine/kv_pages.rs` (update `KV_STAGES` reference)

- [ ] **Step 1: Create `kv_stages.rs`**

```rust
/// KV block stage table with tiered growth.
/// Stages 0-2: 8x growth (cheap to relocate at small sizes)
/// Stages 3-4: 4x growth
/// Stages 5+: 2x growth (conservative at large sizes)
pub const KV_STAGES: &[(u64, usize)] = &[
    (64 * 1024,              1_024),   // Stage 0: 64KB, 1K buckets
    (512 * 1024,             4_096),   // Stage 1: 512KB, 4K buckets
    (4 * 1024 * 1024,        8_192),   // Stage 2: 4MB, 8K buckets
    (32 * 1024 * 1024,      16_384),   // Stage 3: 32MB, 16K buckets
    (128 * 1024 * 1024,     32_768),   // Stage 4: 128MB, 32K buckets
    (512 * 1024 * 1024,     65_536),   // Stage 5: 512MB, 64K buckets
    (1024 * 1024 * 1024,    65_536),   // Stage 6: 1GB, 64K buckets
    (2 * 1024 * 1024 * 1024, 131_072), // Stage 7: 2GB, 128K buckets
    (4 * 1024 * 1024 * 1024, 131_072), // Stage 8: 4GB, 128K buckets
    (8 * 1024 * 1024 * 1024, 262_144), // Stage 9: 8GB, 256K buckets
];

/// Get block size and bucket count for a stage.
/// For stages beyond the table, extrapolates with 2x growth.
pub fn stage_params(stage: usize) -> (u64, usize) {
    if stage < KV_STAGES.len() {
        KV_STAGES[stage]
    } else {
        let (last_size, last_buckets) = KV_STAGES[KV_STAGES.len() - 1];
        let extra = stage - (KV_STAGES.len() - 1);
        (last_size * (1u64 << extra), last_buckets * (1 << extra.min(4)))
    }
}

/// Get the initial stage (stage 0) block size.
pub fn initial_block_size() -> u64 {
    KV_STAGES[0].0
}

/// Get the initial bucket count.
pub fn initial_bucket_count() -> usize {
    KV_STAGES[0].1
}
```

- [ ] **Step 2: Update `kv_pages.rs`**

Change `KV_STAGES` import in `kv_pages.rs` to use the new module:
```rust
use crate::engine::kv_stages::KV_STAGES;
```

Remove the `KV_STAGES` constant from `kv_pages.rs` if it's defined there.

- [ ] **Step 3: Register module, verify, commit**

Add `pub mod kv_stages;` to `mod.rs`.
Run: `cargo build -p aeordb 2>&1 | tail -5`
Commit: `"Extract KV stage table with tiered growth (8x→4x→2x)"`

---

### Task 3: FileHeader Updates

Add new fields to support the single-file layout.

**Files:**
- Modify: `aeordb-lib/src/engine/file_header.rs`

- [ ] **Step 1: Add new fields to FileHeader struct**

Add after `buffer_nvt_offset`:
```rust
    pub hot_tail_offset: u64,
    pub kv_block_stage: u8,
    pub resize_target_stage: u8,
```

- [ ] **Step 2: Update `new()` defaults**

```rust
    hot_tail_offset: 0,
    kv_block_stage: 0,
    resize_target_stage: 0,
```

- [ ] **Step 3: Update `serialize()`**

After the `buffer_nvt_offset` serialization, add:
```rust
    // hot_tail_offset: 8 bytes
    buffer[offset..offset + 8].copy_from_slice(&self.hot_tail_offset.to_le_bytes());
    offset += 8;

    // kv_block_stage: 1 byte
    buffer[offset] = self.kv_block_stage;
    offset += 1;

    // resize_target_stage: 1 byte
    buffer[offset] = self.resize_target_stage;
    offset += 1;
```

- [ ] **Step 4: Update `deserialize()`**

After reading `buffer_nvt_offset`, add:
```rust
    // hot_tail_offset: 8 bytes
    let hot_tail_offset = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // kv_block_stage: 1 byte
    let kv_block_stage = bytes[offset];
    offset += 1;

    // resize_target_stage: 1 byte
    let resize_target_stage = bytes[offset];
    offset += 1;
```

Add the fields to the returned `FileHeader` struct.

- [ ] **Step 5: Fix any existing tests that construct FileHeader**

Search for `FileHeader {` in spec files and add the new fields with default values.

- [ ] **Step 6: Verify and commit**

Run: `cargo build 2>&1 | tail -5`
Run: `cargo test --test entry_header_spec 2>&1 | tail -5`
Commit: `"FileHeader: add hot_tail_offset, kv_block_stage, resize_target_stage"`

---

### Task 4: DiskKVStore — In-File KV Pages

The core rewrite. DiskKVStore reads/writes bucket pages from the main `.aeordb` file instead of a sidecar. Hot buffer writes to hot tail at end of file instead of a separate hot file.

**Files:**
- Modify: `aeordb-lib/src/engine/disk_kv_store.rs` (major rewrite)

- [ ] **Step 1: Remove sidecar file fields**

Remove:
- `kv_path: PathBuf`
- `hot_file: Option<File>`
- `hot_path: Option<PathBuf>`

Replace `kv_file: File` with a shared file handle from the AppendWriter (the main `.aeordb` file). The DiskKVStore will take a cloned file handle + `kv_block_offset` at construction.

Add:
- `kv_block_offset: u64` — where bucket pages start in the file
- `hot_tail_offset: u64` — where the hot tail lives (end of file)

- [ ] **Step 2: Rewrite `create()` and `open()`**

These should take a file handle and offsets instead of a path:

```rust
pub fn create(
    file: File,
    hash_algo: HashAlgorithm,
    kv_block_offset: u64,
    kv_block_length: u64,
    hot_tail_offset: u64,
    stage: usize,
) -> EngineResult<Self>
```

The file handle is a clone of the main database file. KV pages are read/written via `seek()` to `kv_block_offset + bucket_index * page_size`.

- [ ] **Step 3: Rewrite page read/write to use offsets**

All bucket page I/O changes from:
```rust
self.kv_file.seek(SeekFrom::Start(page_offset))?;
```
to:
```rust
self.kv_file.seek(SeekFrom::Start(self.kv_block_offset + page_offset))?;
```

- [ ] **Step 4: Replace hot file with hot tail**

Replace `flush_hot_buffer()` — instead of writing to a separate hot file, call `hot_tail::write_hot_tail()` at `self.hot_tail_offset`.

Replace hot file initialization with hot tail read at startup.

- [ ] **Step 5: Update flush thresholds**

Change:
```rust
const HOT_BUFFER_THRESHOLD: usize = 10;
```
to:
```rust
const HOT_BUFFER_THRESHOLD: usize = 1_000;
```

- [ ] **Step 6: Verify compilation**

Run: `cargo build -p aeordb 2>&1 | tail -10`

This will have cascading compilation errors in StorageEngine — that's expected. Fix signatures to compile, actual integration comes in Task 5.

- [ ] **Step 7: Commit**

```bash
git commit -m "DiskKVStore: in-file KV pages + hot tail, eliminate sidecar files"
```

---

### Task 5: StorageEngine — Single-File Create/Open

Rewrite StorageEngine to create and open the single-file layout.

**Files:**
- Modify: `aeordb-lib/src/engine/storage_engine.rs`

- [ ] **Step 1: Rewrite `create()`**

New create flow:
1. Create the `.aeordb` file
2. Build FileHeader with `kv_block_offset = 256`, `kv_block_length = stage_0_size`, `kv_block_stage = 0`
3. Write header
4. Write zero-filled KV block (stage 0 = 64KB)
5. Write empty NVT after KV block → set `nvt_offset`, `nvt_length`
6. Set `hot_tail_offset` to after NVT (start of append area)
7. Write empty hot tail
8. Re-write header with all offsets populated
9. Create DiskKVStore from the file handle + offsets
10. Create AppendWriter with `current_offset = hot_tail_offset`

- [ ] **Step 2: Rewrite `open()`**

New open flow:
1. Open the `.aeordb` file
2. Read FileHeader → extract all offsets
3. Read NVT from `nvt_offset`
4. Read hot tail from `hot_tail_offset` → load entries into write buffer
5. If hot tail is corrupt → dirty startup (full WAL scan from append area start to hot_tail_offset)
6. Create DiskKVStore from file handle + offsets + loaded buffer
7. Create AppendWriter with `current_offset = hot_tail_offset`
8. If `resize_in_progress` → spawn background resize task

- [ ] **Step 3: Remove all sidecar file code**

Remove:
- `derive_kv_path()` helper
- All `hot_dir` parameters and usage
- KV sidecar file creation/opening/deletion
- Hot file initialization
- All `hot_pattern` matching in open

- [ ] **Step 4: Update AppendWriter integration**

The AppendWriter's `current_offset` must be set to `hot_tail_offset` (not `file.seek(SeekFrom::End(0))`), because the hot tail sits at the end. New entries go BEFORE the hot tail.

On every `append_entry`:
1. Write entry at `hot_tail_offset`
2. Update `hot_tail_offset` to after the entry
3. DiskKVStore re-writes hot tail at new offset

- [ ] **Step 5: Spawn timer flush task**

In `open()` and `create()`, after the engine is constructed, spawn a tokio interval task:

```rust
let engine_weak = Arc::downgrade(&engine);
tokio::spawn(async move {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    loop {
        interval.tick().await;
        let engine = match engine_weak.upgrade() {
            Some(e) => e,
            None => break, // engine dropped, stop
        };
        // try_lock — if writer is busy, skip this tick
        if let Ok(mut kv) = engine.kv_writer.try_lock() {
            if kv.hot_buffer_len() > 0 {
                let _ = kv.flush();
            }
        }
    }
});
```

- [ ] **Step 6: Update header persistence**

After every flush, re-write the file header with updated `hot_tail_offset`, `entry_count`, etc. The header is at offset 0 — seek and overwrite the 256-byte block.

- [ ] **Step 7: Verify compilation and basic tests**

Run: `cargo build 2>&1 | tail -10`
Run: `cargo test --test kv_store_spec 2>&1 | tail -10`

Fix any remaining compilation errors. Some tests may need updating because they create engines with sidecar assumptions.

- [ ] **Step 8: Commit**

```bash
git commit -m "StorageEngine: single-file create/open, no sidecars, timer flush"
```

---

### Task 6: KV Resize — Background WAL Relocation

**Files:**
- Create: `aeordb-lib/src/engine/kv_resize.rs`
- Modify: `aeordb-lib/src/engine/storage_engine.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`

- [ ] **Step 1: Create `kv_resize.rs`**

```rust
use std::io::{Read, Seek, SeekFrom, Write};
use crate::engine::errors::EngineResult;
use crate::engine::kv_stages::stage_params;

/// Batch size for WAL relocation during KV resize.
const RELOCATION_BATCH_SIZE: usize = 64 * 1024 * 1024; // 64MB

/// Calculate the growth zone: bytes between current KV end and target KV end.
pub fn growth_zone(
    kv_block_offset: u64,
    current_length: u64,
    target_stage: usize,
) -> (u64, u64) {
    let (target_length, _) = stage_params(target_stage);
    let zone_start = kv_block_offset + current_length;
    let zone_end = kv_block_offset + target_length;
    (zone_start, zone_end)
}

/// Relocate WAL entries from the growth zone to the end of the append area.
/// Reads in RELOCATION_BATCH_SIZE chunks, appends to hot_tail_offset.
/// Returns the new hot_tail_offset after relocation.
pub fn relocate_batch<F: Read + Write + Seek>(
    file: &mut F,
    zone_start: u64,
    zone_end: u64,
    append_offset: u64, // where to write relocated data (current hot_tail_offset)
) -> EngineResult<u64> {
    let zone_size = (zone_end - zone_start) as usize;
    let mut relocated = 0usize;
    let mut write_offset = append_offset;

    while relocated < zone_size {
        let batch = RELOCATION_BATCH_SIZE.min(zone_size - relocated);
        let read_start = zone_start + relocated as u64;

        // Read batch from growth zone
        let mut buf = vec![0u8; batch];
        file.seek(SeekFrom::Start(read_start))?;
        file.read_exact(&mut buf)?;

        // Write batch at append area
        file.seek(SeekFrom::Start(write_offset))?;
        file.write_all(&buf)?;

        write_offset += batch as u64;
        relocated += batch;
    }

    Ok(write_offset)
}
```

- [ ] **Step 2: Integrate resize trigger into DiskKVStore flush**

In `DiskKVStore::flush()`, when a bucket page is full:
1. Set `resize_in_progress = true` and `resize_target_stage` in the file header
2. Keep the overflowing entry in the write buffer
3. Signal the engine to begin background relocation

- [ ] **Step 3: Background resize task in StorageEngine**

When `resize_in_progress` is detected (on startup or on trigger):
1. Calculate growth zone
2. Call `relocate_batch` in chunks
3. After relocation: update KV page offsets (entries that moved need offset deltas applied to KV bucket pages)
4. Write new KV pages into expanded region
5. Full rehash from WAL scan into new pages
6. Write new NVT
7. Update header, clear `resize_in_progress`

- [ ] **Step 4: Register module, verify, commit**

Add `pub mod kv_resize;` to `mod.rs`.
Run: `cargo build 2>&1 | tail -5`
Commit: `"KV resize: background batch relocation, expand KV block in place"`

---

### Task 7: Integration Tests

**Files:**
- Create: `aeordb-lib/spec/engine/single_file_spec.rs`
- Create: `aeordb-lib/spec/engine/kv_resize_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Create `single_file_spec.rs`**

Tests:
1. **create_single_file** — create a database, verify only one file exists (no `.kv`, no hot file)
2. **store_and_retrieve** — store 100 entries, verify all retrievable via `get_entry`
3. **reopen_preserves_data** — store entries, close, reopen, verify all entries present (hot tail replay)
4. **timer_flush** — store entries, wait 300ms, verify entries moved from hot tail to KV pages (hot tail entry count decreases)
5. **count_flush** — store 1,001 entries rapidly, verify auto-flush triggered
6. **crash_recovery_clean** — store entries, flush, simulate crash (truncate hot tail), reopen — data intact from KV pages
7. **crash_recovery_dirty** — store entries, don't flush, corrupt hot tail, reopen — falls back to WAL scan, data intact
8. **no_sidecar_files** — at no point during any operation should sidecar files exist

- [ ] **Step 2: Create `kv_resize_spec.rs`**

Tests:
1. **resize_triggers_on_overflow** — fill stage 0 until bucket overflow, verify `resize_in_progress` set in header
2. **resize_completes** — trigger resize, wait for background completion, verify KV block expanded
3. **data_intact_after_resize** — store enough entries to trigger resize, verify all entries retrievable after resize
4. **crash_during_resize** — trigger resize, crash mid-relocation, reopen — resize resumes, data intact

- [ ] **Step 3: Register tests in Cargo.toml**

```toml
[[test]]
name = "single_file_spec"
path = "spec/engine/single_file_spec.rs"

[[test]]
name = "kv_resize_spec"
path = "spec/engine/kv_resize_spec.rs"
```

- [ ] **Step 4: Run tests**

Run: `cargo test --test single_file_spec --test kv_resize_spec --test hot_tail_spec 2>&1 | tail -20`
Run: `cargo test 2>&1 | grep "FAILED" || echo "ALL PASS"`

- [ ] **Step 5: Commit**

```bash
git commit -m "Integration tests: single-file lifecycle, crash recovery, KV resize"
```

---

### Task 8: Full Verification

- [ ] **Step 1: Run the complete test suite**

Run: `cargo test 2>&1 | grep "FAILED" || echo "ALL PASS"`

All existing tests must pass — the public API is unchanged.

- [ ] **Step 2: Stress test**

Re-run the existing stress test (or upload a batch of files) and verify:
- Only one `.aeordb` file exists
- No `.kv` or hot sidecar files
- `aeordb verify` reports zero corruption
- Write throughput is comparable or better

- [ ] **Step 3: Verify no sidecar code remains**

```bash
grep -rn "derive_kv_path\|init_hot_file\|hot_path\|kv_path\|\.kv\b" aeordb-lib/src/ --include="*.rs" | grep -v "// \|///\|test\|spec"
```

Should return nothing.

- [ ] **Step 4: Update docs**

Update `docs/src/concepts/storage-engine.md` to describe the single-file layout.

- [ ] **Step 5: Final commit**

```bash
git commit -m "Single-file database: all sidecar code removed, verified"
```
