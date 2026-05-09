# AeorDB Database Write Map

**Date:** 2026-05-09
**Purpose:** Exhaustive documentation of every write operation in the database, including byte-level layout, ordering, locking, fsync behavior, and crash recovery properties.

---

## 1. File Layout (BLAKE3_256, hash_length=32)

```
Offset 0                                File End
|                                            |
[File Header][KV Block      ][WAL Entries...][Hot Tail]
|  256 bytes ||  variable   ||  variable    ||variable|
              |              |               |
              kv_block_offset |              hot_tail_offset
                   (256)      |              (= writer.current_offset)
                              kv_block_offset + kv_block_length
```

### 1.1 File Header (256 bytes, offset 0)

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | magic | `AEOR` (0x41454F52) |
| 4 | 1 | header_version | Currently 1 |
| 5 | 2 | hash_algo | LE u16. BLAKE3_256 = 0x0010 |
| 7 | 8 | created_at | LE i64, ms since epoch |
| 15 | 8 | updated_at | LE i64, ms since epoch |
| 23 | 8 | kv_block_offset | LE u64. Always 256 (after header) |
| 31 | 8 | kv_block_length | LE u64. Depends on stage |
| 39 | 1 | kv_block_version | Currently 1 |
| 40 | 8 | nvt_offset | LE u64. Currently unused (NVT is in-memory) |
| 48 | 8 | nvt_length | LE u64. Currently unused |
| 56 | 1 | nvt_version | Currently 1 |
| 57 | 32 | head_hash | BLAKE3 hash of root directory content |
| 89 | 8 | entry_count | LE u64. Total WAL entries written |
| 97 | 1 | resize_in_progress | 0 or 1 |
| 98 | 8 | buffer_kvs_offset | LE u64. Currently unused |
| 106 | 8 | buffer_nvt_offset | LE u64. Currently unused |
| 114 | 8 | hot_tail_offset | LE u64. Where the hot tail starts |
| 122 | 1 | kv_block_stage | Current KV stage (0-based) |
| 123 | 1 | resize_target_stage | Target stage for pending expansion (0 = none) |
| 124 | 1 | backup_type | 0=normal, 1=export, 2=patch |
| 125 | 32 | base_hash | For patches: source version hash |
| 157 | 32 | target_hash | For patches: destination version hash |
| 189 | 67 | _padding | Zeros (reserved for future fields) |

**Writes to header:**
- `AppendWriter::update_header()` — seeks to 0, writes 256 bytes, calls `sync_data()`
- `AppendWriter::update_file_header()` — same as above (alias)
- Called during: HEAD update, shutdown, KV expansion

### 1.2 KV Block (offset 256, variable length)

Divided into bucket pages. Each page holds up to 32 KV entries.

**KV Stage Sizes:**

| Stage | Block Size | Buckets (BLAKE3) | Page Size |
|-------|-----------|-----------------|-----------|
| 0 | 64 KB | 49 | 1,314 bytes |
| 1 | 512 KB | 399 | 1,314 bytes |
| 2 | 4 MB | 3,192 | 1,314 bytes |
| 3 | 32 MB | 25,532 | 1,314 bytes |
| 4 | 128 MB | 102,130 | 1,314 bytes |

**Page format (1,314 bytes for BLAKE3):**

| Offset | Size | Description |
|--------|------|-------------|
| 0 | 2 | entry_count (LE u16, max 32) |
| 2 | 41×N | N entries, each: hash(32) + type_flags(1) + offset(8) |
| 2+41×32 | remaining | Zero padding |

**KV entry (41 bytes for BLAKE3):**

| Offset | Size | Field |
|--------|------|-------|
| 0 | 32 | hash (key hash) |
| 32 | 1 | type_flags (lower 4 bits = type, upper 4 bits = flags) |
| 33 | 8 | offset (LE u64, position in WAL) |

**type_flags values:**
- `0x0` = Chunk
- `0x1` = FileRecord
- `0x2` = Directory
- `0x3` = Deletion
- `0x4` = Snapshot
- `0x5` = Void
- `0x6` = Head
- `0x7` = Fork
- `0x8` = Version
- `0x9` = Symlink
- `0x80` = DELETED flag (ORed with type)

**Writes to KV pages:**
- `DiskKVStore::flush()` — reads each modified page, merges new entries via `upsert_in_page`, writes back, calls `sync_data()` once after all pages
- `DiskKVStore::flush_no_snapshot()` — same but no snapshot publish
- `DiskKVStore::finalize_expansion()` — zeroes ALL pages, then bulk_insert + flush_no_snapshot

### 1.3 WAL Entries (after KV block, before hot tail)

Each entry is self-describing:

**Entry format:**

| Offset | Size | Field |
|--------|------|-------|
| 0 | 4 | magic (LE u32, 0x0AE012DB) |
| 4 | 1 | entry_version (currently 0) |
| 5 | 1 | entry_type |
| 6 | 1 | flags |
| 7 | 2 | hash_algo (LE u16) |
| 9 | 1 | compression_algo |
| 10 | 1 | encryption_algo |
| 11 | 4 | key_length (LE u32) |
| 15 | 4 | value_length (LE u32) |
| 19 | 8 | timestamp (LE i64, ms since epoch) |
| 27 | 4 | total_length (LE u32, entire entry including header) |
| 31 | 32 | hash (BLAKE3 of entry_type + key + value) |
| 63 | key_length | key bytes |
| 63+key_length | value_length | value bytes |

**Total: 63 + key_length + value_length bytes**

**Writes to WAL:**
- `AppendWriter::append_entry()` — seeks to `current_offset`, writes header+key+value, advances `current_offset`. **No fsync** (relies on hot tail for crash recovery).
- `AppendWriter::append_entry_with_compression()` — same with compression flag
- `AppendWriter::write_entry_at()` — writes at specific offset (for voids), calls `sync_all()`
- `AppendWriter::write_void_at()` — writes void entry at specific offset, calls `sync_all()`

### 1.4 Hot Tail (at hot_tail_offset, end of file)

Journal of recent KV entries for crash recovery.

**Format:**

| Offset | Size | Field |
|--------|------|-------|
| 0 | 5 | magic: `AE 01 7D B1 0C` |
| 5 | 4 | entry_count (LE u32) |
| 9 | 4 | crc32 of entry_count bytes |
| 13 | 41×N | N entries, each: hash(32) + type_flags(1) + offset(8) |

**Writes to hot tail:**
- `DiskKVStore::flush_hot_buffer()` — writes ALL write_buffer entries to hot tail at `self.hot_tail_offset`, calls `set_len(end)` to truncate, calls `sync_data()`
- `DiskKVStore::flush()` — when overflow → writes overflow entries to hot tail (line 461). When all fit → writes empty hot tail (line 481)
- `hot_tail::write_hot_tail()` — generic writer function

---

## 2. File Handles

The database file is opened by THREE separate handles:

| Handle | Owner | Purpose | Mode |
|--------|-------|---------|------|
| `AppendWriter.file` | StorageEngine.writer (RwLock) | WAL appends, header updates | read+write |
| `AppendWriter.reader` | StorageEngine.writer (RwLock) | pread for entry reads | read-only |
| `DiskKVStore.db_file` | StorageEngine.kv_writer (Mutex) | KV page read/write, hot tail | read+write |

**CRITICAL:** These are separate file handles to the SAME file. Writes via one handle are visible to reads via another handle ONLY after `sync_data()` / `sync_all()` flushes kernel buffers.

---

## 3. Locks

| Lock | Type | Protects | Acquisition Order |
|------|------|----------|-------------------|
| `StorageEngine.writer` | `RwLock<AppendWriter>` | WAL appends, header reads/writes | First (always) |
| `StorageEngine.kv_writer` | `Mutex<DiskKVStore>` | KV pages, hot tail, write buffer | Second (after writer) |

**Lock order MUST be: writer first, then kv_writer.** Violating this causes deadlock.

---

## 4. Write Paths

### 4.1 store_entry_internal (single entry write)

```
Caller: store_file, store_chunk, finalize_file, etc.
Locks: writer(WRITE) + kv_writer(LOCK)

Steps:
1. writer.append_entry(type, key, value, flags)
   → seek to current_offset
   → write header(63) + key + value (NO FSYNC)
   → advance current_offset
   → increment file_header.entry_count

2. kv.set_hot_tail_offset(writer.current_offset())
   → updates kv.hot_tail_offset (but NOT on disk)

3. kv.insert(KVEntry { hash, type_flags, offset })
   → write_buffer.insert(hash, entry)
   → hot_buffer.push(entry)
   → IF hot_buffer.len() >= 32:
       flush_hot_buffer() → writes ALL write_buffer to hot tail on disk (sync_data)
   → IF write_buffer.len() >= WRITE_BUFFER_THRESHOLD:
       flush() → writes modified pages to disk (sync_data)
              → may trigger resize_to_next_stage()
   → ELSE:
       publish_buffer_only() → updates in-memory snapshot

4. Check kv.needs_expansion
   → if Some(stage): drop locks, call expand_kv_block_online()

Durability: WAL entry is on disk (but NOT fsynced). KV entry is in
write_buffer (memory). Hot buffer entry accumulates until threshold.
```

### 4.2 flush_batch (batched directory writes)

```
Caller: update_parent_directories
Locks: writer(WRITE) + kv_writer(LOCK)

Steps:
1. FOR EACH entry in batch:
   writer.append_entry(type, key, value, 0)
   kv.set_hot_tail_offset(writer.current_offset())
   → NO FSYNC per entry

2. FOR EACH entry in batch:
   kv.insert(KVEntry { hash, type_flags, offset })
   → may trigger hot_buffer flush or page flush

3. kv.flush_hot_buffer()  ← NEW (as of today's fix)
   → writes ALL write_buffer entries to hot tail
   → sync_data()

4. Drop locks
5. Check kv.needs_expansion
```

### 4.3 flush_batch_and_update_head (directory propagation + HEAD)

```
Caller: update_parent_directories (at root level)
Locks: writer(WRITE) + kv_writer(LOCK)

Steps:
1-2. Same as flush_batch

3. writer.update_file_header(header with new head_hash)
   → seek to 0, write 256 bytes, sync_data()

4. kv.flush_hot_buffer()  ← NEW (as of today's fix)
   → sync_data()

5. Drop locks
6. Check kv.needs_expansion
```

### 4.4 DiskKVStore::insert (single KV entry)

```
Caller: store_entry_internal, flush_batch, flush_batch_and_update_head
Lock: kv_writer already held by caller

Steps:
1. write_buffer.insert(hash, entry)  [MEMORY]
2. IF is_new: entry_count += 1  [MEMORY]
3. hot_buffer.push(entry)  [MEMORY]
4. IF hot_buffer.len() >= 32:
     flush_hot_buffer()  [DISK: hot tail write + sync_data]
5. IF write_buffer.len() >= WRITE_BUFFER_THRESHOLD:
     flush()  [DISK: KV page writes + sync_data]
     → may trigger resize_to_next_stage()
   ELSE:
     publish_buffer_only()  [MEMORY: update ArcSwap snapshot]
```

### 4.5 DiskKVStore::flush (KV page flush)

```
Caller: insert() threshold, shutdown(), Drop
Lock: kv_writer already held

Steps:
1. Group write_buffer entries by NVT bucket
2. FOR EACH modified bucket:
   a. Read page from disk (seek + read_exact)
   b. Deserialize existing entries
   c. Upsert new entries (replace if same hash, append if space)
   d. If page full → entry goes to overflow_entries
   e. Serialize page
   f. Write page to disk (seek + write_all)
3. sync_data()  [ONE sync for ALL modified pages]
4. write_buffer.clear()

5. IF overflow_entries not empty:
   a. publish_snapshot_incremental()
   b. resize_to_next_stage()
   c. IF resize succeeded: re-insert overflow, recursive flush()
   d. IF resize blocked (needs expansion):
      - Re-insert overflow to write_buffer
      - Write ALL write_buffer to hot tail  [DISK: sync_data]
      - Set needs_expansion = Some(target_stage)
      - publish_buffer_only()

6. IF no overflow:
   a. flush_hot_buffer()
   b. Write empty hot tail (clears old data)
   c. publish_snapshot_incremental()
```

### 4.6 DiskKVStore::flush_hot_buffer

```
Lock: kv_writer already held

Steps:
1. Collect ALL write_buffer values (not just hot_buffer)
2. hot_tail::write_hot_tail(db_file, hot_tail_offset, all_entries, hash_length)
   → seek to hot_tail_offset
   → write magic(5) + count(4) + crc32(4) + entries(41×N)
3. db_file.set_len(end)  [truncate stale trailing data]
4. db_file.sync_data()
5. hot_buffer.clear()
```

**CRITICAL NOTE:** The hot tail contains ALL write_buffer entries, not just the hot_buffer additions. This is because the hot tail is the COMPLETE crash recovery journal — it must contain everything that's in the write buffer but not yet in KV pages.

### 4.7 Shutdown

```
Caller: StorageEngine::shutdown() / Drop

Steps:
1. Lock kv_writer
2. kv.flush()
   → writes all write_buffer entries to KV pages
   → may trigger resize_to_next_stage() → needs_expansion
   → if all fit: writes empty hot tail
   → if overflow: writes overflow to hot tail
3. kv.flush_hot_buffer()
   → writes remaining write_buffer to hot tail
4. Unlock kv_writer

5. Lock kv_writer again (separate scope)
6. Read kv.hot_tail_offset() and kv.len()
7. Unlock kv_writer

8. Lock writer
9. Update header: hot_tail_offset, entry_count
10. writer.update_header() [DISK: seek 0, write 256, sync_data]
11. writer.sync_all() [DISK: full fsync including metadata]
12. Unlock writer
```

**BUG (potential):** Between step 2 (kv.flush) and step 3 (kv.flush_hot_buffer), if kv.flush triggered a resize that set needs_expansion, the expansion never happens during shutdown. The needs_expansion flag is in-memory only and is lost. On next startup, the KV pages may be incomplete (overflow entries only in hot tail), but the hot tail has them, so they're recoverable.

**BUG (confirmed, fixed today):** Step 2 (kv.flush) may write the hot tail at a stale hot_tail_offset if overflow → resize blocked. The `set_hot_tail_offset` guard now prevents backward movement, but the offset could still be incorrect if the writer advanced past what the KV thinks is the hot_tail.

### 4.8 Startup (open_internal)

```
Steps:
1. Acquire file lock (lock_path)
2. Open AppendWriter (reads header)
3. Set writer offset to header.hot_tail_offset (if > 0)

4. Check for pending KV expansion:
   IF resize_target_stage > kv_block_stage:
   → drop writer
   → expand_kv_block (offline relocation)
   → reopen writer, re-read header

5. Read hot tail entries from hot_tail_offset

6. Open DiskKVStore:
   a. Read all KV pages into memory
   b. Count entries from page headers
   c. Pre-populate write_buffer with hot tail entries
   d. Create initial ReadSnapshot (pages + write_buffer)

7. Scan WAL for void entries (void_manager)

8. Build StorageEngine struct

9. IF needs_kv_rebuild:
   → rebuild_kv() — full WAL scan, re-populate KV

10. Initialize counters from KV snapshot
```

**CRITICAL:** The hot tail entries loaded at step 5 go into the write_buffer at step 6b. They are NOT flushed to KV pages during startup. They remain in the write buffer until a flush is triggered (by threshold or explicit call). The in-memory snapshot includes them, so reads work. But if the server shuts down before they're flushed to pages, they must survive via the hot tail again.

---

## 5. Hard Link Directory Entries

After the directory propagation optimization, directory entries at path-based keys (`dir_key`) store a 32-byte content hash instead of the full directory data.

**Detection:** If `get_entry(dir_key)` returns a value of exactly `hash_length` bytes (32 for BLAKE3), it's a hard link. The value IS the content hash. Read the full data from `get_entry(content_hash)`.

**Write path (update_parent_directories):**
```
FOR EACH directory level (child → root):
  1. read_directory_data(dir_key)  [follows hard links, checks cache]
  2. Modify children list (insert/update child entry)
  3. Serialize new directory content → dir_value
  4. Hash dir_value → content_key
  5. batch.add(DirectoryIndex, content_key, dir_value)  [full data]
  6. batch.add(DirectoryIndex, dir_key, content_key)    [32-byte hard link]
  7. cache_dir_content(content_key, dir_value)  [in-memory cache]

AT ROOT:
  8. flush_batch_and_update_head(batch, content_key)
     → ALL entries written to WAL
     → ALL entries inserted into KV
     → HEAD updated in file header
     → Hot tail flushed
```

**CRITICAL:** Both the content entry (step 5) and the hard link (step 6) are in the SAME batch. They are flushed together. If either is lost, the hard link is dangling.

---

## 6. Fsync Points

| Operation | Fsync Call | What It Syncs |
|-----------|-----------|---------------|
| append_entry | NONE | WAL entry NOT synced (relies on hot tail) |
| update_header | sync_data() | File header (256 bytes) |
| KV page flush | sync_data() | All modified KV pages (one sync for batch) |
| flush_hot_buffer | sync_data() | Hot tail (write_buffer contents) |
| write_void_at | sync_all() | Void entry (includes metadata) |
| shutdown sync_all | sync_all() | Entire file including metadata |

**Durability guarantees:**
- WAL entries: durable only after hot tail or KV page flush
- KV entries: durable after KV page flush OR hot tail flush
- File header: durable after update_header's sync_data
- Hot tail: durable after flush_hot_buffer's sync_data

---

## 7. Crash Scenarios

### 7.1 Crash during append_entry (WAL write)
- Entry may be partially written (torn write)
- No KV entry exists yet
- Entry scanner will skip it (invalid magic or hash mismatch)
- No data loss (entry never made it to KV)

### 7.2 Crash after append_entry but before KV insert
- WAL has the entry, KV doesn't
- Hot tail doesn't have it (hot_buffer not flushed)
- On restart: entry is in WAL but invisible to KV
- Recovery: `verify --repair` scans WAL, rebuilds KV

### 7.3 Crash after KV insert but before hot tail flush
- WAL has the entry
- KV write_buffer has the entry (memory only)
- Hot tail does NOT have the entry
- On restart: entry is in WAL, not in KV, not in hot tail
- Recovery: same as 7.2

### 7.4 Crash after hot tail flush
- WAL has the entry
- Hot tail has the KV entry
- On restart: hot tail entries loaded into write_buffer → visible immediately
- No data loss

### 7.5 Crash during KV page flush
- Some pages written, others not
- Hot tail may be stale (pre-flush snapshot)
- On restart: KV pages partially updated, hot tail fills gaps
- May leave stale entries in pages that were written

### 7.6 Crash during shutdown
- kv.flush() may have partially completed
- kv.flush_hot_buffer() may not have run
- writer.update_header() may not have run
- On restart: depends on which steps completed
- Hot tail may be stale → some entries lost from KV
- Recovery: `verify --repair`

---

## 8. Known Issues

### 8.1 WAL entries are not fsynced individually
WAL appends do NOT call fsync. If the OS crashes (not just the process), WAL entries since the last fsync may be lost entirely. The hot tail provides process-crash recovery but NOT OS-crash recovery for the WAL itself.

### 8.2 Hot tail offset can become stale
The `DiskKVStore.hot_tail_offset` is updated via `set_hot_tail_offset()` after each WAL append. However, during `flush()` overflow handling (line 461), the hot tail is written at `self.hot_tail_offset`. If this value is stale (not updated from a concurrent write path), the hot tail may overwrite WAL data. The backward-movement guard mitigates this but doesn't guarantee correctness if the offset was never updated.

### 8.3 Hot buffer threshold delay
With HOT_BUFFER_THRESHOLD=32, up to 31 entries can be in memory without a hot tail flush. If the process crashes with 31 unflushed entries, those KV entries are lost (though the WAL entries survive and are recoverable via repair).

### 8.4 DiskKVStore::Drop calls flush() which can trigger resize
The `Drop` impl calls `flush()` which can trigger `resize_to_next_stage()`. During drop, the StorageEngine may already be partially dismantled. If resize triggers `needs_expansion`, the expansion never happens (no one checks it after Drop). Overflow entries stay in the write buffer which is about to be dropped.

### 8.5 Separate file handles
The writer and KV store use separate file handles. After one handle writes and syncs, the other handle's reads should see the new data (fsync flushes kernel buffers). However, this depends on OS-level guarantees. On some systems, page cache coherency between file descriptors is not guaranteed without explicit synchronization.

---

## 9. Write Sequence for a Single File Store

To store `/docs/file.txt` with content "hello" (3 levels deep):

```
1. store_file_internal():
   Lock: writer(W) + kv(M)

   a. store_chunk("hello")
      → WAL: append Chunk entry at offset A
      → KV: insert(chunk_hash, offset=A)

   b. store FileRecord
      → WAL: append FileRecord at offset B (identity_key)
      → WAL: append FileRecord at offset C (file_path_key)
      → KV: insert(identity_key, offset=B)
      → KV: insert(file_path_key, offset=C)

   Unlock

2. update_parent_directories("/docs/file.txt", child_entry):

   a. Level: parent="/docs"
      → read_directory_data(dir_key_docs)
      → modify children, serialize → dir_value_docs
      → content_key_docs = hash(dir_value_docs)
      → batch.add(content_key_docs, dir_value_docs)   [full content]
      → batch.add(dir_key_docs, content_key_docs)      [32-byte hard link]
      → cache(content_key_docs, dir_value_docs)

   b. Level: parent="/" (ROOT)
      → read_directory_data(dir_key_root)
      → modify children, serialize → dir_value_root
      → content_key_root = hash(dir_value_root)
      → batch.add(content_key_root, dir_value_root)
      → batch.add(dir_key_root, content_key_root)
      → cache(content_key_root, dir_value_root)

      → flush_batch_and_update_head(batch, content_key_root):
        Lock: writer(W) + kv(M)

        WAL writes (NO fsync):
          append(DirectoryIndex, content_key_docs, dir_value_docs)  → offset D
          append(DirectoryIndex, dir_key_docs, content_key_docs)    → offset E
          append(DirectoryIndex, content_key_root, dir_value_root)  → offset F
          append(DirectoryIndex, dir_key_root, content_key_root)    → offset G

        KV inserts (memory):
          kv.insert(content_key_docs, offset=D)
          kv.insert(dir_key_docs, offset=E)
          kv.insert(content_key_root, offset=F)
          kv.insert(dir_key_root, offset=G)

        Header update (fsync):
          header.head_hash = content_key_root
          writer.update_file_header()  → sync_data()

        Hot tail flush (fsync):
          kv.flush_hot_buffer()  → writes ALL write_buffer to hot tail
                                 → sync_data()

        Unlock

3. Indexing pipeline (if config exists):
   → may write index files via store_file
   → each triggers its own update_parent_directories cycle
```

**Total disk writes for one file at 3 levels:**
- 5 WAL appends (chunk + 2 FileRecords + 4 DirectoryIndex entries in batch)
  - Wait, the chunk and FileRecords are separate store_entry calls (step 1)
  - Actually: chunk(1) + FileRecord identity(1) + FileRecord path(1) + dir batch(4) = 7 WAL appends
- 1 file header update (sync_data)
- 1 hot tail flush (sync_data)
- 0-1 KV page flushes (if threshold reached)

**Fsync count: 2 minimum** (header update + hot tail flush)
**Fsync count before today's fix: 1** (header update only — hot tail might never flush)
