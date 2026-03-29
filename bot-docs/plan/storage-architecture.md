# Storage Architecture — The Final Design

**Parent:** [Master Plan](./master-plan.md)
**Status:** In Design — Finalized Architecture (Revision 2)

This document supersedes the earlier storage-engine.md for architectural decisions. The implementation details in storage-engine.md (redb backends, etc.) remain relevant for the current Phase 1-4 code, but this document describes the target architecture.

---

## Two Primitives: Chunks and B-Trees

### The Chunk

A chunk is a block of raw data, identified by its BLAKE3 content hash. Chunks are immutable — once written, they never change. They have no structural awareness — no pointers, no links. Just data.

```
Chunk:
  [header: 33 bytes]
    format_version: u8          // engine format version (starts at 1)
    created_at:     i64         // millisecond timestamp (8 bytes)
    updated_at:     i64         // millisecond timestamp (8 bytes)
    reserved:       [u8; 16]    // reserved for future use

  [data: chunk_size - 33 bytes]
    raw bytes                   // the content

chunk_hash = BLAKE3(data)       // hash covers DATA only, not header
```

- **Header:** Engine metadata. Format version for future-proofing, timestamps for auditing, reserved space for extensibility. NOT included in the hash.
- **Data:** The actual content. Included in the hash. Immutable.
- **Chunk size:** Configurable, power-of-two (default 256KB). Data capacity = chunk_size - 33 bytes.
- **No pointers.** Chunks do not know about other chunks. They are dumb data blocks. All structure lives in the B-tree.

The hash is the chunk's "name" — its address in the chunk store. Identical data = identical hash = automatic deduplication.

### The B-Tree

The B-tree is the structural backbone. It serves TWO purposes:

1. **Directory structure** — maps names to files (which chunks compose a file, in what order)
2. **File structure** — an ordered list of chunk hashes within each file entry

B-tree nodes are themselves stored as chunks in the chunk store. The B-tree is COW (copy-on-write) — modifications create new nodes, old nodes are preserved. This gives us versioning for free.

---

## Layer Separation

```
Layer 1: Chunk Store (redb)
  chunk_hash → chunk_bytes
  Dumb key-value storage. Doesn't know about files, directories, or structure.
  redb provides ACID, crash recovery, and fast hash lookups.

Layer 2: Filesystem (B-trees stored as chunks)
  Paths, directories, file composition, versioning.
  All stored AS chunks in Layer 1, but Layer 1 doesn't know or care.
```

redb is the block device. The B-tree filesystem sits on top. If we ever swap redb for raw files, S3, or Ceph, only Layer 1 changes.

---

## Files

A file is an index entry in a directory B-tree. The entry contains an ordered list of chunk hashes that compose the file's data.

### Small Files (inline chunk list)

For files with few chunks, the chunk hash list is stored directly in the B-tree leaf entry:

```
B-tree leaf entry:
  name: "abc123"
  entry_type: File
  chunks: [hash_A, hash_B, hash_C]     // inline, no indirection
  metadata: { document_id, created_at, updated_at, content_type }
```

### Large Files (overflow chunk list)

For files with many chunks (hash list exceeds leaf entry space), the chunk list is stored in a separate chunk (or B-tree of chunks for massive files):

```
B-tree leaf entry:
  name: "huge_video.mp4"
  entry_type: File
  chunk_list_reference: hash_X          // points to a chunk containing the hash list
  metadata: { ... }
```

The referenced chunk contains packed hashes (32 bytes each). At 256KB chunk size, one overflow chunk holds ~8,000 hashes = ~2GB of file data. For larger files, the overflow is itself a B-tree of chunk lists.

### Why Not Linked Lists

An earlier design used linked lists (next/prev pointers in chunk headers). This was abandoned because:

- **Immutable headers + linked lists = cascade on every write.** Inserting or appending a chunk requires updating the previous chunk's `next` pointer, which changes its header, which means a new chunk, which requires updating ITS predecessor... cascading all the way to the head. For a 1000-chunk file, appending one byte creates 1000 new chunks.
- **Mutable headers + linked lists = versioning is impossible.** If you mutate a chunk's header, any version snapshot pointing to that chunk sees the modified chain, not the original. Versions become stale silently.
- **B-trees solve both problems.** COW B-tree nodes only cascade to the root of that subtree (O(log n) nodes), not the entire chain. Old nodes are preserved, so versioning is automatic.

---

## Directories

A directory is a per-directory B-tree. Each B-tree node is a chunk stored in the chunk store.

### B-Tree Node Format

Branch node (chunk data):
```
[format_version: u8]
[node_type: u8 = 0x01]
[num_keys: u16]
[keys: serialized name strings]
[children: chunk hashes of child nodes]
```

Leaf node (chunk data):
```
[format_version: u8]
[node_type: u8 = 0x02]
[num_entries: u16]
[entries: serialized IndexEntries]
```

### Index Entries

Each entry in a leaf node:

```rust
struct IndexEntry {
  name: String,                    // human-readable: "abc123", ".config"
  entry_type: EntryType,           // File, Directory, HardLink
  chunks: ChunkList,               // inline hash list or overflow reference
  document_id: Uuid,               // unique identifier
  created_at: DateTime<Utc>,       // when created
  updated_at: DateTime<Utc>,       // when last modified
  content_type: Option<String>,    // MIME type of the data
}

enum EntryType {
  File,        // chunks compose raw content
  Directory,   // chunks compose a child B-tree root
  HardLink,    // shares another entry's chunk list (dedup at file level)
}

enum ChunkList {
  Inline(Vec<ChunkHash>),          // small files: hashes stored directly
  Overflow(ChunkHash),             // large files: points to chunk containing hash list
}
```

### Per-Directory B-Trees

Each directory has its OWN B-tree, not one flat B-tree for the entire filesystem.

```
/ (B-tree: small, top-level entries)
  └── "myapp" → Directory → child B-tree root chunk
        └── "users" → Directory → child B-tree root chunk (could be huge)
              ├── "abc123" → File → [chunk_A, chunk_B, chunk_C]
              ├── "def456" → File → [chunk_D]
              ├── ".config" → File → [chunk_E]
              └── ".indexes" → Directory → child B-tree root chunk
```

**Why per-directory, not flat:**
- Writing one file only COWs nodes in that directory's B-tree
- Small directories stay small, large directories scale independently
- The root B-tree doesn't become a hot path bottleneck
- Aligns with permission resolution (per path segment)

### Path Traversal

Resolving `/myapp/users/abc123`:
1. Open root B-tree → look up "myapp" → get Directory entry
2. Open myapp's B-tree → look up "users" → get Directory entry
3. Open users' B-tree → look up "abc123" → get File entry
4. Read chunks from the entry's chunk list

Cost: O(depth × log n) where n is the largest directory at each level.

### Auto-Creation (mkdir -p)

Writing to a path automatically creates intermediate directories. `PUT /myapp/deep/nested/path/file.json` creates `myapp/`, `deep/`, `nested/`, `path/` if they don't exist.

---

## Versioning: B-Tree COW = Free Snapshots

Versioning is a natural consequence of COW B-trees. Every modification creates new nodes. Old nodes are preserved. A "version" is just a saved B-tree root hash.

### How It Works

```
State 1: root_A → ... → leaf with "abc123" → [chunk_A, chunk_B, chunk_C]

Modify abc123 (replace chunk_B with chunk_X):
  1. Create new leaf node with [chunk_A, chunk_X, chunk_C]
  2. COW branch nodes up to root → new root_B
  3. Old root_A still points to old leaf with [chunk_A, chunk_B, chunk_C]

State 2: root_B → ... → leaf with "abc123" → [chunk_A, chunk_X, chunk_C]

Version 1 = root_A
Version 2 = root_B
chunk_A and chunk_C are shared between versions.
```

### Bases and Diffs (I-Frames and P-Frames)

For storage efficiency, not every version needs to be a full root snapshot:

**Base (I-frame):** The complete B-tree root hash + metadata. Everything needed to reconstruct the full state.

**Diff (P-frame):** A minimal delta — which B-tree nodes changed, which chunks were added/removed. Applied on top of a base (or previous diff) to reconstruct state.

```
Base₁ → diff → diff → diff → Base₂ → diff → diff → diff
```

**Restoring a version:**
1. Find nearest base at or before the target version
2. Apply diffs sequentially
3. Done

**When to create a new base:**
- After N diffs (configurable)
- When cumulative diff size exceeds a threshold
- On explicit user request
- Engine can decide automatically

### The Entire Database State Is One Hash

The root B-tree root hash captures the ENTIRE database state at a point in time. Every file, every directory, every index, every config — all reachable from that one hash. Saving that hash = complete snapshot. Restoring that hash = complete rollback.

---

## Hard Links

A hard link shares another entry's chunk list. The data chunks are stored once. Both entries point to the same chunks.

```
Directory A:
  "report.pdf" → File → [chunk_X, chunk_Y]

Directory B:
  "shared_report.pdf" → HardLink → [chunk_X, chunk_Y]   // same chunks
```

Moving or renaming either entry doesn't affect the other. The chunks exist independently of the directory entries that reference them.

---

## Streaming (No Full File Reads)

File reads are ALWAYS streamed. There is no "load entire file into memory" operation. The engine provides chunk-by-chunk streaming:

1. Read the chunk list from the B-tree entry
2. Yield chunks one at a time
3. The caller processes each chunk as it arrives

This prevents:
- Memory exhaustion from opening large files
- Attack vectors (malicious large file crashes the server)
- Accidental "oopsies" (opening a file you shouldn't have)

Data flows on request. Pumping streams. Durability and resilience are non-negotiable.

---

## Repair and Recovery

The B-tree + chunk architecture is highly repairable:

- **Corrupt chunk:** BLAKE3 hash mismatch on read. The chunk is identified as bad. The file is partially readable (other chunks are fine). Replication can provide a clean copy.
- **Corrupt B-tree node:** Rebuild from sibling nodes or from a known-good version snapshot.
- **Lost root:** Scan all chunks, identify B-tree nodes by format version + node type bytes, reconstruct bottom-up.
- **Version recovery:** Walk backward through versions to find the last clean state.

Corruption is always **local**. A bad chunk or bad node doesn't cascade. The content-addressed hashes detect corruption immediately on read.

---

## Dot-Path Conventions

System paths use dot-prefix (Unix hidden file convention):

| Path | Purpose | Default Permissions |
|---|---|---|
| `.config` | Path configuration (parsers, validators, permissions) | admin: full, users: read |
| `.indexes` | Search index data | engine: full, admin: full, users: read |
| `.system` | System-level data | admin: full, users: none |

These are regular files/directories. The engine gives them special meaning by convention. Permissions control access, not naming rules. Users can create their own dot-paths with no special engine meaning.

---

## Mandatory Document Metadata

Every file has engine-managed metadata stored in the B-tree index entry:

| Field | Type | Description |
|---|---|---|
| `document_id` | UUID v4 | Unique identifier |
| `created_at` | Timestamp | When created |
| `updated_at` | Timestamp | When last modified |
| `content_type` | Option<String> | MIME type of the data |

No soft-delete. Delete is real delete. Recovery is via version restore.

---

## Summary

| Concept | Implementation |
|---|---|
| Chunk | Header (format version, timestamps, reserved) + Data (BLAKE3 hashed). Immutable. Dumb. |
| File | B-tree index entry with ordered chunk hash list (inline or overflow). |
| Directory | Per-directory COW B-tree. Nodes stored as chunks. |
| Index Entry | Name + type + chunk list + metadata. |
| Hard Link | Entry sharing another entry's chunk list. |
| Version | Saved B-tree root hash. The entire database state is one hash. |
| Base | Full root snapshot (I-frame). |
| Diff | Minimal B-tree delta (P-frame). |
| Streaming | Always. No full-file memory loads. Ever. |
| Physical storage | redb (Layer 1): dumb hash→bytes. Swappable. |

---

## Relationship to Existing Code

The current Phase 1-4 implementation uses redb with a simpler model. Migration path:

1. Current: redb-backed key-value storage with content-addressed chunks
2. Target: COW B-tree filesystem with chunk store on redb
3. The `ChunkStorage` trait provides the abstraction boundary
4. Existing 380 tests continue to validate the current implementation
5. New filesystem module (`src/filesystem/`) built alongside existing code
