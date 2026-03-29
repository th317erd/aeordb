# AeorDB — Sprint 2: The Real Filesystem Layer

Async collaboration document. Wyatt reviews and adds inline comments.

---

## Sprint Goal

Build the actual filesystem layer on top of redb's chunk store. Replace flat document storage with linked-chunk files, per-directory B-trees, and path-based access. Also clean up existing code to remove soft-delete.

---

## Task 1: Clean Up Soft-Delete

Remove soft-delete from the engine. Delete is real delete. Recovery is via versioning.

**Changes:**
- Remove `is_deleted` from `Document` struct and `MetadataUpdates`
- Remove `is_deleted` from the binary serialization envelope in `redb_backend.rs`
- Remove soft-delete logic from `RedbStorage` (delete actually removes the record)
- Remove undelete from `update_document_metadata`
- Update `list_documents` — remove the `include_deleted` parameter
- Update `get_document` — no more soft-delete filtering
- Update HTTP handlers — DELETE returns 200 on success, 404 if not found. No more soft-delete semantics.
- Update ALL affected tests (storage, HTTP, auth middleware)

<!-- WYATT: This is a breaking change to the existing API. The `?include_deleted=true` query param goes away. Any test that tests soft-delete/undelete behavior gets removed or rewritten. OK to proceed? -->

<!-- 
Oh I love this! I love that we simplified everything with this one decision. Yes! _Please proceed_.
 -->

---

## Task 2: Chunk Primitive Revision

Update the chunk to have a proper header with linked-list pointers.

**New chunk format:**
```
[header: 64 bytes]
  next_chunk:     [u8; 32]   // zeros if last
  previous_chunk: [u8; 32]   // zeros if first

[data: chunk_size - 64 bytes]
  raw content bytes
```

**Changes to existing chunk code:**
- `Chunk` struct gets `next_chunk` and `previous_chunk` fields (both `Option<ChunkHash>`)
- `chunk_hash = BLAKE3(data)` — hash covers data ONLY, not header
- `ChunkStorage` trait may need `update_chunk_header()` method (to update next/prev without changing the hash)
- `ChunkConfig` — data capacity per chunk = `chunk_size - 64`
- Serialization/deserialization includes header

<!-- 
Claude, I want everything to have a version number... the data itself is not what I am talking about. THAT is already all hashes. I am talking about an "engine" version. For example, what if we decide in the future to upgrade chunk header formats in the future? How does the engine know what format it is reading? I think it is important right up front to add version numbers to the "format". I am honestly totally fine with a single byte, and right now we are at version "1". I doubt we will ever have more than 255 "format" versions for our chunk data.
 -->

**New file:** `aeordb-lib/src/storage/chunk_header.rs`

```rust
pub struct ChunkHeader {
  pub next_chunk: Option<ChunkHash>,
  pub previous_chunk: Option<ChunkHash>,
}
```

<!-- WYATT: The header is 64 bytes (two 32-byte hashes). With a 256KB chunk size, that's 0.025% overhead — negligible. With a 4KB minimum chunk size, it's 1.6% — still fine. Any concerns? -->

<!--
Yes, I have concerns... primarily on the simplicity. We are relying on this data structure pretty heavily. I think at the very least we need a version number (for the format/structure itself), and I'd also like to see a created_at at and updated_at millisecond resolution timestampts here as well. That is 8 * 2 bytes, + 1 byte for versioning, and that puts us at 17 bytes used. We still have 32 - 17 = RESERVED space left... assuming of course that we do 32 + 32 + 32 (three blocks) for the header. This would allow us to have a next/previous pointers, and a metainfo block that contains version, created_at, updated_at, and some more reserved space.
 -->

**Tests:**
```
spec/chunks/chunk_header_spec.rs
  - test_chunk_with_no_siblings_has_zero_hashes
  - test_chunk_with_next_sibling
  - test_chunk_with_both_siblings
  - test_chunk_hash_excludes_header
  - test_update_header_preserves_data_hash
  - test_header_serialization_roundtrip
  - test_data_capacity_is_chunk_size_minus_header
```

---

## Task 3: Linked List Files

Build the linked-list file abstraction on top of chunks.

**New file:** `aeordb-lib/src/filesystem/linked_file.rs`

```rust
pub struct LinkedFile {
  pub first_chunk: ChunkHash,
  pub total_size: u64,
}
```

**Operations:**
- `write_file(storage, data) -> LinkedFile` — split data into chunks, link them via headers, return the first chunk hash
- `read_file(storage, first_chunk) -> Vec<u8>` — walk the linked list, concatenate data
- `stream_read(storage, first_chunk) -> impl Iterator<Item = &[u8]>` — streaming chunk-by-chunk read
- `append_to_file(storage, first_chunk, data) -> LinkedFile` — find last chunk, link new chunks
- `file_size(storage, first_chunk) -> u64` — walk and sum data sizes (or cache in metadata)
- `truncate_file(storage, first_chunk) -> Vec<ChunkHash>` — unlink all chunks, return their hashes for cleanup

<!-- WYATT: For `read_file`, I'm returning `Vec<u8>` (load entire file into memory). For large files, `stream_read` is the right call. Should we make `read_file` cap at a certain size and force streaming above that, or trust the caller? I'm leaning toward trusting the caller — they know their workload. -->

<!-- 
No, let's not. We should ALWAYS stream. Period. If we decide that read_file can read data into memory, then this becomes an attack vector, and opens up the potential for accidental mishaps too, such as crashing your database server, because you opened a file that you shouldn't have. We should have streams, pumping streams, where data only flows upon request. Durablilty ans resilliency is a vital component of this design. We SHOULD NOT bake in crash triggers, or possible easy points of abuse or "oopsie"!
 -->

**Tests:**
```
spec/filesystem/linked_file_spec.rs
  - test_write_and_read_small_file (fits in one chunk)
  - test_write_and_read_multi_chunk_file
  - test_write_and_read_exact_chunk_boundary
  - test_write_and_read_empty_file
  - test_chunks_are_properly_linked (verify next/prev pointers)
  - test_stream_read_yields_correct_chunks
  - test_append_to_existing_file
  - test_truncate_file_returns_chunk_hashes
  - test_file_size_accurate
  - test_large_file_many_chunks (1MB+ to verify chain integrity)
  - test_single_byte_file
  - test_data_hash_unchanged_after_relinking
```

---

## Task 4: Index Entry and Directory B-Tree

Build the per-directory B-tree index.

**New file:** `aeordb-lib/src/filesystem/index_entry.rs`

```rust
pub struct IndexEntry {
  pub name: String,
  pub entry_type: EntryType,
  pub first_chunk: ChunkHash,
  pub document_id: Uuid,
  pub created_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
  pub content_type: Option<String>,
}

pub enum EntryType {
  File,
  Directory,
  HardLink,
}
```

<!-- WYATT: I'm putting document metadata (document_id, created_at, updated_at, content_type) directly in the index entry. This means listing a directory gives you all metadata without reading file chunks — fast `ls`. The trade-off is slightly larger index entries, but since metadata is small (UUID + two timestamps + optional string), this seems like the right call. Agree? -->

<!-- 
Ha! We arrived at nearly the same conclusion! Love it!
 -->

**New file:** `aeordb-lib/src/filesystem/directory.rs`

B-tree implementation where each node is a chunk stored in the chunk store.

```rust
pub struct Directory {
  pub root_chunk: ChunkHash,  // B-tree root node
}
```

**Operations:**
- `create_directory(storage) -> Directory` — create empty directory (single leaf node)
- `insert_entry(storage, directory, entry) -> Directory` — insert an index entry, returns new root (COW)
- `get_entry(storage, directory, name) -> Option<IndexEntry>` — look up by name
- `remove_entry(storage, directory, name) -> (Directory, Option<IndexEntry>)` — remove entry, return new root + removed entry
- `list_entries(storage, directory) -> Vec<IndexEntry>` — walk leaf nodes via linked list
- `list_entries_range(storage, directory, start, end) -> Vec<IndexEntry>` — range scan

**B-tree node format (stored as chunk data):**

Branch node:
```
[node_type: u8 = 0x01]
[num_keys: u16]
[keys: serialized strings]
[children: chunk hashes]
```

Leaf node:
```
[node_type: u8 = 0x02]
[num_entries: u16]
[entries: serialized IndexEntries]
```

Leaf nodes use chunk headers (next/prev) for sibling linkage. Branch nodes may not need sibling links (traversal goes through parent).

**B-tree properties:**
- Order/fanout configurable (default: fit as many keys as possible per chunk)
- Keys are entry names, sorted lexicographically
- COW: modifications create new nodes, old nodes preserved for versioning
- Split/merge follows standard B-tree algorithms

<!-- WYATT: B-tree implementation is the most complex piece in this sprint. I'm planning to build a clean, standalone B-tree module that happens to store nodes as chunks. Standard algorithms — nothing exotic. The COW property comes naturally because we store new chunks rather than modifying existing ones. -->

<!-- 
Sounds good to me.
 -->

**Tests:**
```
spec/filesystem/directory_spec.rs
  - test_create_empty_directory
  - test_insert_and_get_entry
  - test_insert_multiple_entries_sorted
  - test_get_nonexistent_returns_none
  - test_remove_entry
  - test_remove_nonexistent_returns_none
  - test_list_entries_returns_all
  - test_list_entries_sorted_lexicographically
  - test_list_entries_range
  - test_insert_many_entries_causes_split
  - test_remove_entries_causes_merge
  - test_cow_old_root_still_valid (insert creates new root, old root unchanged)
  - test_hard_link_entry_shares_first_chunk
  - test_directory_entry_points_to_another_directory
  - test_large_directory_many_entries (1000+ entries)
  - test_index_entry_metadata_preserved
  - test_entry_with_content_type
  - test_entry_without_content_type
```

---

## Task 5: Path Traversal

Resolve filesystem paths segment by segment.

**New file:** `aeordb-lib/src/filesystem/path_resolver.rs`

```rust
pub struct PathResolver {
  root_directory: Directory,
  storage: Arc<dyn ChunkStorage>,
}
```

**Operations:**
- `resolve_path(path: &str) -> Result<ResolvedPath>` — walk from root, segment by segment, return the final entry + all intermediate directories
- `create_path(path: &str) -> Result<ResolvedPath>` — resolve, creating intermediate directories as needed (like `mkdir -p`)
- `store_file(path: &str, data: &[u8], content_type: Option<&str>) -> Result<IndexEntry>` — create intermediate dirs + store file
- `read_file(path: &str) -> Result<(Vec<u8>, IndexEntry)>` — resolve path, read file data
- `delete_file(path: &str) -> Result<IndexEntry>` — resolve path, remove entry, return removed entry
- `list_directory(path: &str) -> Result<Vec<IndexEntry>>` — resolve path to directory, list entries

```rust
pub struct ResolvedPath {
  pub segments: Vec<(String, Directory)>,  // each segment with its directory
  pub entry: Option<IndexEntry>,           // the final entry (if it exists)
}
```

<!-- WYATT: Path traversal creates intermediate directories automatically on write (like `mkdir -p`). This matches your "zero ceremony" philosophy — you don't need to pre-create directory structure. Just write to `/myapp/deep/nested/path/file.json` and all intermediate directories appear. Reading a nonexistent path returns 404. -->

<!-- 
I like this. Thank you. One would always assume that you would have pre-created the directory, if you are going to be configuring indexes and the such... but still, I like the "let's help you out" paradigm.
 -->

**Tests:**
```
spec/filesystem/path_resolver_spec.rs
  - test_resolve_root
  - test_resolve_single_segment
  - test_resolve_deep_path
  - test_resolve_nonexistent_returns_none
  - test_store_file_creates_intermediate_directories
  - test_store_and_read_file_roundtrip
  - test_delete_file
  - test_delete_nonexistent_returns_error
  - test_list_root_directory
  - test_list_nested_directory
  - test_list_empty_directory
  - test_overwrite_existing_file
  - test_store_file_updates_updated_at
  - test_path_with_dot_prefix (system paths)
  - test_hard_link_across_directories
```

---

## Task 6: Wire HTTP Layer to Filesystem

Replace the current redb-backed document storage with the new filesystem layer in the HTTP handlers.

**Changes:**
- HTTP routes become true path-based: `PUT /any/path/here` stores a file, `GET /any/path/here` reads it, etc.
- Remove the `build_table_name` function (no more "database:table" concatenation)
- AppState gets `Arc<PathResolver>` (or `Arc<Filesystem>` or whatever we call the top-level struct)
- CRUD handlers call PathResolver instead of RedbStorage for document operations
- RedbStorage stays for system tables (API keys, config, etc.) — or we migrate those to the filesystem too

<!-- WYATT: The big question is whether we migrate system tables (API keys, signing keys, etc.) to the filesystem now or later. My instinct: leave them in redb for now. They're working, they're tested, and migrating them is complexity we don't need in this sprint. We can move them later when the filesystem is proven. Agree? -->

<!-- 
Agreed. I am not too concerned about this at the moment.
 -->

**Tests:**
- Update all existing HTTP tests to use the new path-based routes
- Existing auth tests should continue working (system tables stay in redb)

---

## Task 7: Base+Diff Versioning

Build the versioning layer.

**New file:** `aeordb-lib/src/filesystem/versioning.rs`

**Base (I-frame):**
- Snapshot of the root directory's B-tree root chunk hash + timestamp + metadata
- Stored as a file at `/.system/versions/base_{id}`

**Diff (P-frame):**
- List of changes since last base or diff: chunks added, chunk headers modified, index entries changed
- Stored as a file at `/.system/versions/diff_{id}`

**Operations:**
- `create_base(filesystem) -> Version` — snapshot current state
- `create_diff(filesystem, since_version) -> Version` — capture changes since a version
- `restore_version(filesystem, version_id) -> Result<()>` — apply base + diffs to restore state
- `list_versions() -> Vec<Version>`
- `auto_base_check() -> bool` — should we create a new base? (too many diffs, too much cumulative change)

<!-- WYATT: Versioning is the most "can be deferred" task in this sprint. The filesystem works without it — versioning adds recovery and history. If the sprint is getting too large, this is the task I'd push to a later sprint. Your call. -->

<!-- 
I agree with you, and at the same time I don't. I am already having some typhoons in my head, because things aren't lining up the way I expected with our current design and versioning. I am thinking this is actually something we should figure out _up front_, at the same time. I am totally okay with getting the file system setup and stable before we get in too deep, but I think we need to be planning for version right now, because I think it will very much impact the way we design our file system.
 -->

**Tests:**
```
spec/filesystem/versioning_spec.rs
  - test_create_base_snapshot
  - test_create_diff_captures_changes
  - test_restore_from_base
  - test_restore_from_base_plus_diffs
  - test_list_versions_ordered
  - test_auto_base_triggers_after_threshold
  - test_version_metadata_stored
  - test_restore_old_version_data_matches
```

---

## Resolved: Versioning / Immutability Problem

Wyatt identified a critical flaw in the linked-list chunk design:

**The problem:** If chunk headers (next/prev pointers) are immutable, inserting or appending a chunk cascades new copies all the way back to the head of the chain. A 1000-chunk file append = 1000 new chunks. If headers are mutable, old version snapshots become stale because the chain is modified in place.

**The solution:** Drop linked lists entirely. Chunks are dumb data blocks with NO pointers. File structure (which chunks, in what order) lives in the COW B-tree index entries.

- Small files: chunk hash list inline in the B-tree leaf entry
- Large files: chunk hash list in an overflow chunk referenced by the entry
- Modifying a file: update the chunk list, COW the B-tree leaf → branch → root. O(log n) new nodes, not O(n).
- Versioning: save the old B-tree root hash. Old nodes preserved. Old chunks preserved. Restore = load old root.

**The entire database state is one hash** — the root B-tree root hash. Save it = full snapshot. Restore it = full rollback.

**Chunk header simplified to:** format_version (u8) + created_at (i64) + updated_at (i64) + reserved (16 bytes) = 33 bytes. No next/prev pointers.

See updated `bot-docs/plan/storage-architecture.md` (Revision 2) for full details.

---

## Updated Module Structure

```
aeordb-lib/src/filesystem/
  mod.rs                    — module declarations, Filesystem top-level struct
  chunk_header.rs           — ChunkHeader (format version, timestamps, reserved)
  index_entry.rs            — IndexEntry, EntryType, ChunkList (inline/overflow)
  directory.rs              — COW B-tree directory + file chunk ordering
  path_resolver.rs          — path traversal, mkdir -p, store/read/delete/list
  versioning.rs             — base+diff version management (B-tree root snapshots)
```

Wire up in `lib.rs`: `pub mod filesystem;`

---

## Updated Sprint Ordering

```
Task 1 (soft-delete cleanup)     — independent, do first to clean the slate
Task 2 (chunk headers)           — simplified (no next/prev, just metadata)
Task 3 (B-tree with file storage)— B-tree handles BOTH directories and file chunk lists
Task 4 (path resolver)           — depends on Task 3
Task 5 (HTTP wiring)             — depends on Task 4
Task 6 (versioning)              — depends on Task 3 (COW B-tree root snapshots)
```

Task 6 (versioning) should be designed alongside Tasks 3-4, even if implementation is deferred.

---

## Resolved Questions

1. **Soft-delete removal** — Approved. Proceed.
2. **Versioning** — Design now, build alongside filesystem. NOT deferred.
3. **System tables** — Leave in redb for now. Migrate later.
4. **Streaming** — Always. No full-file memory loads. Ever. Non-negotiable.
5. **Chunk headers** — Format version + timestamps + reserved. No linked list pointers.
6. **Linked lists** — Dropped. B-tree owns all structure.

---

*Sprint plan reviewed and approved. Ready to build on Wyatt's signal.

<!--
Previous discussion preserved below for reference:

You did a fantastic job! I have some concerns with our headers, and versioning. Let me share with you:

A   ->   B   ->   C ->   F:
data...data...data....data

We have chunks A, B, C, and F each with a pointer pointing backwards, and forwards, next and prev.

So, the theory goes: in order to retain versions, we just have to point to a specific "A" hash, and the version of the entire chain will be retained.

However, it isn't quite so simple. The only way for that to be true, is for the headers to be **immutable**.

Think about that for a moment. Let's say you have a file like so:

A   ->   B   ->   C ->   F:
data...data...data....data

And you need to insert or append a Z:
A   ->   B   ->    Z  ->   C ->   F:
data...data... Zdata...data....data

Well, now B needs to be updated, as well as C. However, they are immutable. So we create B2 and C2.

Yay! But hold on... A still points to B, but it really needs to be point to B2, but A is immutable. So we create an A2...

Do you see where this is going?

**Versioning** is a vitally important thing for us to think about now.

We can decide that headers are immutable. Great! Makes the file system way easier. However, we just shot ourselves in the foot.

Maybe this need not be our design at all. Maybe we think of something else entirely. But I think we need to think, and I have thought enough to think thinking is the thing we should consider. 😉
 -->
 