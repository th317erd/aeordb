# AeorDB Database Write Map

**Last verified:** 2026-08-05
**Purpose:** Exhaustive documentation of every write operation in the database, including byte-level layout, ordering, locking, fsync behavior, and crash recovery properties.

---

## 1. File Layout (BLAKE3_256, hash_length=32)

```
Offset 0                                File End
|                                            |
[Header A][Header B][KV Block      ][WAL Entries...][Hot Tail]
| 256 bytes|| 256 bytes||  variable   ||  variable    ||variable|
              |              |               |
              kv_block_offset |              hot_tail_offset
                   (512)      |              (= writer.current_offset)
                              kv_block_offset + kv_block_length
```

### 1.1 File Header (two 256-byte A/B slots, offsets 0 and 256)

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | magic | `AEOR` (0x41454F52) |
| 4 | 1 | header_version | Currently 1 |
| 5 | 2 | hash_algo | LE u16. BLAKE3_256 = 0x0010 |
| 7 | 8 | sequence | LE u64 A/B publication sequence |
| 15 | 8 | created_at | LE i64, ms since epoch |
| 23 | 8 | updated_at | LE i64, ms since epoch |
| 31 | 8 | kv_block_offset | LE u64. 512 for current v3 files |
| 39 | 8 | kv_block_length | LE u64. Depends on stage/boundary alignment |
| 47 | 1 | kv_block_version | Currently 1 |
| 48 | 8 | nvt_offset | LE u64. Currently unused |
| 56 | 8 | nvt_length | LE u64. Currently unused |
| 64 | 1 | nvt_version | Currently 1 |
| 65 | 32 | head_hash | BLAKE3 hash of root directory content |
| 97 | 8 | entry_count | LE u64. Current merged KV entry count |
| 105 | 1 | resize_in_progress | Pre-relocation phase marker |
| 106 | 8 | buffer_kvs_offset | LE u64. Reserved |
| 114 | 8 | buffer_nvt_offset | LE u64. Reserved |
| 122 | 8 | hot_tail_offset | LE u64. Where the hot tail starts |
| 130 | 1 | kv_block_stage | Current completed KV stage |
| 131 | 1 | resize_target_stage | Selected recovery target stage |
| 132 | 1 | backup_type | 0=normal, 1=export, 2=patch |
| 133 | 32 | base_hash | For patches: source version hash |
| 165 | 32 | target_hash | For patches: destination version hash |
| 197 | 55 | _padding | Zeros (reserved for future fields) |
| 252 | 4 | crc32 | CRC over bytes 0..252 |

**Writes to header:**
- Every publication goes through the shared `DurabilityCoordinator`.
- The dependency/data barrier runs before the inactive slot is written.
- The inactive slot is written positionally, followed by an authority barrier and read-back.
- Only after successful read-back does `AppendWriter` select the new in-memory slot.
- Namespace transactions mutate the in-memory header and publish it only from their grouped hard-authority completion.
- Timer/direct publishers acquire the namespace/frontier guard; the timer defers on contention.

### 1.2 KV Block (offset 256, variable length)

Divided into bucket pages. Each page holds up to 32 KV entries.

**KV Stage Sizes:**

| Stage | Nominal Block Size |
|-------|--------------------|
| 0 | 64 KiB |
| 1 | 512 KiB |
| 2 | 4 MiB |
| 3 | 32 MiB |
| 4 | 128 MiB |
| 5 | 512 MiB |
| 6-9 | 1, 2, 4, and 8 GiB |

Bucket count is derived from the current page width. Expansion may extend the
stored block beyond the nominal size so its end lands on a validated WAL entry
boundary.

**Page format (1,450 bytes for BLAKE3):**

| Offset | Size | Description |
|--------|------|-------------|
| 0 | 4 | page magic |
| 4 | 4 | crc32 over the complete page with this field zeroed |
| 8 | 2 | entry_count (LE u16, max 32) |
| 10 | 45×N | hash(32) + type_flags(1) + offset(8) + total_length(4) |
| remainder | variable | Zero padding |

**KV entry (45 bytes for BLAKE3):**

| Offset | Size | Field |
|--------|------|-------|
| 0 | 32 | hash (key hash) |
| 32 | 1 | type_flags (lower 4 bits = type, upper 4 bits = flags) |
| 33 | 8 | offset (LE u64, position in WAL) |
| 41 | 4 | total_length (LE u32, complete WAL entry extent) |

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
- `flush()` and the no-publish bulk path share `prepare_page_flush` plus `apply_prepared_page_flush`.
- Preparation reads through the bounded provider, validates magic/CRC/framing, computes every replacement and overflow, and reserves retained generations before mutation.
- Application writes exact pages, crosses one coordinator data barrier, commits the provider generation, and only then publishes/clears buffered state.
- A pre-overwrite error leaves disk and the published view unchanged. A failure after the first overwrite is `PostMutationDurabilityFailure` and latches the database read-only at the engine boundary.

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
| 0 | 5 | magic: `AE 01 7D B1 0D` |
| 5 | 1 | top-level format version |
| 6 | 4 | write_count (LE u32) |
| 10 | 4 | void_count (LE u32) |
| 14 | 4 | crc32 of bytes 0..14 |
| 18 | 46×N | v1 write records: version + hash(32) + flags + offset + total_length |
| following | 13×M | v1 Void records: version + offset + size |

**Writes to hot tail:**
- `DiskKVStore::flush_hot_buffer()` — writes all `write_buffer` entries plus the complete sanitized Void snapshot at `self.hot_tail_offset`, then uses the shared recoverable data barrier
- `DiskKVStore::flush()` — when overflow remains, writes that recoverable state and requests expansion; when pages contain all writes, retains the Void snapshot with zero write records
- `hot_tail::write_hot_tail()` — generic writer function

---

## 2. File Handles

The database file is opened by THREE separate handles:

| Handle | Owner | Purpose | Mode |
|--------|-------|---------|------|
| `AppendWriter.file` | StorageEngine.writer (RwLock) | WAL appends, header updates | read+write |
| `AppendWriter.reader` | StorageEngine.writer (RwLock) | pread for entry reads | read-only |
| `DiskKVStore.db_file` | StorageEngine.kv_writer (Mutex) | KV page read/write, hot tail | read+write |

These handles share the same kernel file/cache state, so ordinary coherent reads
see completed writes without an fsync. `sync_data`/`sync_all` are durability
barriers, not visibility primitives. AeorDB still coordinates publication and
read-back explicitly; it does not use cross-descriptor visibility as proof that
bytes survived a crash.

---

## 3. Locks

| Lock | Type | Protects | Acquisition Order |
|------|------|----------|-------------------|
| `StorageEngine.writer` | `RwLock<AppendWriter>` | WAL appends, header reads/writes | First (always) |
| `StorageEngine.kv_writer` | `Mutex<DiskKVStore>` | KV pages, hot tail, write buffer | Second (after writer) |
| `StorageEngine.namespace_write_lock` | Reentrant-by-thread mutex protocol | Mutable path/directory/HEAD publication | Before writer/KV for namespace writes |
| `EngineOperationTracker` maintenance gate | Mutex + condition variable | Exclusive full-layout reinterpretation | Drain ordinary operations before writer/KV |

**Lock order MUST be: writer first, then kv_writer.** Violating this causes deadlock.

Hard header publication additionally requires namespace authority and an idle
hard frontier. A transaction admits its hard ticket while it still owns
namespace authority. Direct publishers wait before acquiring the namespace;
reentrant non-transaction publishers fail closed rather than deadlocking while
holding it. KV expansion takes the exclusive maintenance gate, then the same
namespace/frontier authority, before any marker or page is changed.

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

3. kv.insert(KVEntry { hash, type_flags, offset, total_length })
   → write_buffer.insert(hash, entry)
   → hot_buffer.push(entry)
   → IF hot_buffer.len() >= 512:
       flush_hot_buffer() → writes ALL write_buffer to hot tail through the coordinator barrier
   → IF write_buffer.len() >= WRITE_BUFFER_THRESHOLD:
       flush() → prepares all pages, reserves retained generations, writes exact replacements, crosses one coordinator barrier
              → may request a later layout stage
   → ELSE:
       publish_buffer_only() → updates in-memory snapshot

4. Check kv.needs_expansion
   → outside a transaction: take the request, drop locks, call the sole engine expansion owner
   → a pre-mutation refusal restores a still-valid future-stage request; a
     post-marker failure latches read-only and leaves startup recovery in charge
   → inside a transaction: leave the request queued until hard completion

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

3. Drop locks
4. Run a queued expansion only when transaction depth is zero
```

### 4.3 flush_batch_and_update_head (directory propagation + HEAD)

```
Caller: update_parent_directories (at root level)
Locks: writer(WRITE) + kv_writer(LOCK)

Steps:
1-2. Same as flush_batch

3. If a namespace transaction is active, update HEAD only in memory.
   Otherwise publish the inactive A/B slot through the shared hard-authority plan.

4. Drop locks
5. Run a queued expansion only when transaction depth is zero. Transactional
   callers run it after their grouped hard commit, never before.
```

### 4.4 DiskKVStore::insert (single KV entry)

```
Caller: store_entry_internal, flush_batch, flush_batch_and_update_head
Lock: kv_writer already held by caller

Steps:
1. write_buffer.insert(hash, entry)  [MEMORY]
2. IF is_new: entry_count += 1  [MEMORY]
3. hot_buffer.push(entry)  [MEMORY]
4. IF hot_buffer.len() >= 512:
     flush_hot_buffer()  [DISK: hot tail write + coordinator data barrier]
5. IF write_buffer.len() >= WRITE_BUFFER_THRESHOLD:
     flush()  [prepare-before-overwrite + retained-generation admission + one barrier]
     → may set `needs_expansion`
   ELSE:
     publish_buffer_only()  [MEMORY: update ArcSwap snapshot]
```

### 4.5 DiskKVStore::flush (KV page flush)

```
Caller: insert() threshold, shutdown(), Drop
Lock: kv_writer already held

Steps:
1. Group `write_buffer` entries by NVT bucket.
2. Read each current page through the bounded provider and validate magic,
   CRC, entry count, offsets, and framing.
3. Build every replacement and collect overflow without touching disk.
4. Begin one provider update, reserving all old page generations. Any pressure,
   corruption, or I/O error here returns before mutation.
5. Write all replacement pages and cross one coordinator data barrier.
6. Commit the replacement generation, update exact type counts, and retain only
   overflow in `write_buffer`.
7. If overflow remains, publish it in the recoverable hot tail and request the
   next layout stage. Otherwise retain the current Void snapshot in an empty-write
   hot tail and publish the new bounded snapshot.
```

### 4.6 DiskKVStore::flush_hot_buffer

```
Lock: kv_writer already held

Steps:
1. Collect ALL write_buffer values (not just hot_buffer)
2. Serialize the complete recoverable payload: all `write_buffer` entries plus
   the current sanitized Void snapshot.
   → seek to hot_tail_offset
   → write versioned/checksummed hot-tail bytes
3. db_file.set_len(end)  [truncate stale trailing data]
4. Cross the shared coordinator data barrier
5. hot_buffer.clear()
```

**CRITICAL NOTE:** The hot tail contains ALL write_buffer entries, not just the hot_buffer additions. This is because the hot tail is the COMPLETE crash recovery journal — it must contain everything that's in the write buffer but not yet in KV pages.

### 4.7 Shutdown

```
Caller: StorageEngine::shutdown() / Drop

Steps:
1. Reject new operations and wait, with a bounded timeout, for active operations
   and every durability waiter/driver to drain.
2. Flush dirty indexes through their shared buffer owner.
3. Lock `kv_writer`; run the same prepare-before-overwrite page flush used by
   live writes, then publish any remaining recoverable hot tail.
4. Release `kv_writer` before recording any serious failure so emergency spill
   can inspect volatile KV state.
5. Read `hot_tail_offset` and the exact merged entry count.
6. Publish those values through the inactive A/B header slot using the shared
   hard-authority coordinator and read-back.
7. On any serious failure, latch read-only, preserve first/latest evidence, try
   emergency spill, and return an error. A repeated blocked shutdown does not
   start another flush.
```

### 4.8 Startup (open_internal)

```
Steps:
1. Acquire file lock (lock_path)
2. Open AppendWriter (reads header)
3. Set writer offset to header.hot_tail_offset (if > 0)

4. Recover a selected expansion phase:
   - `resize_in_progress=true`: validate/retry relocation from the old layout.
   - `resize_in_progress=false` with a later target: relocated WAL is already
     durable; finish page rebuild/final publication without relocating twice.
   - Any malformed/out-of-range phase aborts startup rather than warning-success.

5. Read hot tail entries from hot_tail_offset

6. Open DiskKVStore:
   a. Create a zero-retention positioned-read page provider
   b. Validate and count one KV page at a time, releasing each page immediately
   c. Pre-populate write_buffer with hot tail entries and void masks
   d. Create an initial provider-backed ReadSnapshot

7. Scan WAL for void entries (void_manager)

8. Build StorageEngine struct

9. IF needs_kv_rebuild:
   → rebuild_kv() — full WAL scan, re-populate KV

10. Initialize counters from KV snapshot
11. Resolve strict memory configuration and atomically replace the bootstrap
    provider with the process-coordinator-backed bounded cache before ready
    admission. If configuration remains unresolved during the transition
    release, clean pages stay zero-retention and retained generations use a
    private 8 MiB fail-closed bootstrap bound.
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

## 6. Durability Barriers

| Commit step | Coordinator operation | Purpose |
|-------------|-----------------------|---------|
| dependency append | `DependencyAppend` | Write hot-tail/dependency bytes before authority |
| data barrier | `DataBarrier` | Make WAL, KV pages, or hot-tail dependencies recoverable |
| inactive-slot write | `AuthorityWrite` / `HeaderAb` | Publish the next A/B authority candidate |
| authority barrier | `AuthorityBarrier` | Make the selected slot durable |
| read-back | `AuthorityReadback` | Prove the exact serialized slot before acknowledgement |
| shutdown | `ShutdownFlush` | Attribute and latch any final-flush failure |

Native `sync_data`, `sync_all`, positional writes, and read-back are confined to
the platform/coordinator adapters and the architecture allowlist. WAL appends are
intentionally buffered, but no successful namespace mutation is acknowledged
until its grouped hard plan has made both the WAL/hot-tail dependency and A/B
authority durable.

---

## 7. Crash Direction

- Before a transaction hard commit, the previously selected A/B header remains
  authoritative. In-memory HEAD/backup changes are not independently published.
- A torn or failed inactive slot loses by CRC/sequence selection; the old slot
  remains valid.
- KV page replacements retain old generations before overwrite, cross the data
  barrier before new-generation publication, and classify every later failure as
  durability-critical.
- Expansion first selects a pre-relocation marker. It then copies only complete
  validated WAL entries, writes the relocated hot tail, and crosses a barrier.
  A distinct relocation-durable marker is selected before old bytes are zeroed
  or rehashed. Startup retries/finalizes according to the selected phase.
- Failures before any layout/header mutation preserve the current view and do
  not latch merely for resource pressure or malformed input. Failures at or
  after uncertain mutation latch the database read-only and preserve spill
  evidence for explicit repair.
- Unacknowledged WAL bytes may require dirty-start scanning, but acknowledged
  namespace state never relies on an unbarriered hot tail.

---

## 8. Remaining Transition Boundaries

- P2b-3 still has to move directory, generic server, index, query, parser/plugin,
  task, GC, repair, and maintenance allocations from observation-only accounting
  to enforced process-coordinator reservations and eviction.
- The v3 KV block still uses fixed-size bucket pages. The v4 migration replaces
  its index artifacts and NVT semantics; this document describes the protected
  v3 write path that remains live during that migration.
- `DiskKVStore::Drop` is only a last-resort best-effort cleanup. Truthful
  acknowledgement belongs to explicit engine operations and `shutdown()`; Drop
  errors are logged and never converted into success.

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

        Transaction-local header update:
          header.head_hash = content_key_root
          writer.set_header_in_memory()  → no authority publication yet

        Unlock

3. Transaction hard completion (while ticket ordering is still protected by
   namespace authority):
   → admit the v3 hard-authority plan
   → serialize the complete hot tail as the dependency
   → one data barrier for WAL/hot-tail recovery state
   → write the inactive A/B header slot
   → one authority barrier and exact read-back
   → only then acknowledge the file write and emit counters/SSE

4. Indexing pipeline (if config exists):
   → may write index files via store_file
   → each triggers its own update_parent_directories cycle
```

**Typical physical work for one file at 3 levels:**
- 7 WAL appends: chunk(1) + FileRecord identity/content/path materializations + directory batches (exact count varies with dedup and current FileRecord version).
- 1 complete recoverable hot-tail dependency write.
- 1 inactive A/B header-slot write plus read-back.
- 0-1 KV page flushes (if threshold reached)

**Barrier count: 2 for the grouped hard plan** (dependency/data barrier, then
authority barrier). KV pressure may add one recoverable page barrier before the
transaction completion; it does not add another independent header authority.
