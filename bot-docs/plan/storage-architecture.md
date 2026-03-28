# Storage Architecture — The Final Design

**Parent:** [Master Plan](./master-plan.md)
**Status:** In Design — Finalized Architecture

This document supersedes the earlier storage-engine.md for architectural decisions. The implementation details in storage-engine.md (redb backends, etc.) remain relevant for the current Phase 1-4 code, but this document describes the target architecture.

---

## One Primitive: The Chunk

Everything in aeordb is stored as chunks. There is exactly one storage primitive.

```
Chunk:
  [header]
    next_chunk:     [u8; 32]   // hash of next sibling (zeros if last)
    previous_chunk: [u8; 32]   // hash of previous sibling (zeros if first)
  [data]
    raw bytes                  // the content (hashed for integrity/naming)

chunk_hash = BLAKE3(data)      // hash covers DATA only, not header
```

- **Header:** Navigation metadata. next/prev pointers for linked list traversal. NOT included in the hash. Mutable by the engine.
- **Data:** The actual content. Included in the hash. Immutable once written.
- **Chunk size:** Configurable, power-of-two (default 256KB). The data portion is chunk_size minus header size (64 bytes for two 32-byte hashes).

The hash is the chunk's "name" — its address in the chunk store. But it's a content hash, so identical data = identical name = automatic deduplication.

---

## Files Are Linked Lists of Chunks

A file is a linked list. You need the hash of the first chunk, and you can walk the entire file by following `next_chunk` pointers.

```
File "abc123":
  chunk_A: [prev: 0000, next: hash_B][...data...]
  chunk_B: [prev: hash_A, next: hash_C][...data...]
  chunk_C: [prev: hash_B, next: 0000][...data...]
```

- **No file index needed.** No list of hashes stored separately. The file IS the chain.
- **No size limits.** A file can be infinite — just keep linking chunks.
- **Streaming reads.** Read chunk, follow `next`, repeat. No need to load a hash list first.
- **Streaming writes.** Append = create new chunk, link it to the previous last chunk.
- **Random access.** To seek to byte offset N: chunk_index = N / data_per_chunk, walk N chunks from the start (or use an index for O(1) seek if needed).

---

## Directories Are B-Trees of Chunks

A directory is a B-tree. Each B-tree node is a chunk.

### B-Tree Structure

```
Branch node chunk:
  [header: next, prev]
  [data: sorted keys + child chunk hashes]

Leaf node chunk:
  [header: next, prev]         ← sibling links for range scans
  [data: sorted index entries]
```

### Index Entries

Each entry in a leaf node:

```rust
struct IndexEntry {
  name: String,                // human-readable: "abc123", ".config", "photos"
  entry_type: EntryType,       // File, Directory, HardLink
  first_chunk: ChunkHash,      // hash of the first chunk of the target
}

enum EntryType {
  File,        // first_chunk starts a linked list of raw content chunks
  Directory,   // first_chunk starts a B-tree (another directory)
  HardLink,    // first_chunk points to another file's first chunk (shared data)
}
```

### Per-Directory B-Trees

Each directory has its OWN B-tree. Not one flat B-tree for the entire filesystem.

```
/ (B-tree: small, contains top-level entries)
  └── "myapp" → Directory → B-tree root chunk
        └── "users" → Directory → B-tree root chunk (could be huge)
              ├── "abc123" → File → first data chunk
              ├── "def456" → File → first data chunk
              ├── ".config" → File → first config chunk
              └── ".indexes" → Directory → B-tree root chunk
```

**Why per-directory, not flat:**
- Writing one file only COWs nodes in that directory's B-tree, not the entire index
- The root B-tree doesn't become a hot path bottleneck
- Small directories stay small, large directories scale independently
- Aligns with permission resolution (per path segment)

### Path Traversal

Resolving `/myapp/users/abc123`:
1. Open root B-tree → look up "myapp" → get Directory entry
2. Open myapp's B-tree → look up "users" → get Directory entry
3. Open users' B-tree → look up "abc123" → get File entry
4. Follow first_chunk → read file data

Cost: O(depth × log n) where n is the largest directory at each level.

### Range Scans (Listing)

Leaf nodes are linked via chunk headers (next/prev). To list all entries in a directory: find the leftmost leaf, walk the linked list. No need to traverse up and down the tree.

---

## Hard Links

A hard link points directly to a file's first chunk hash. Unlike a symlink (which points to a path and breaks if the path changes), a hard link always resolves because it points to the data itself.

```
Directory A:
  "report.pdf" → File → chunk_X

Directory B:
  "shared_report.pdf" → HardLink → chunk_X   // same first chunk!
```

Both entries point to the same chunk chain. The data is stored once. Moving or renaming entries in either directory doesn't break the other.

---

## Versioning: Bases and Diffs

Versioning uses a video-compression-inspired model: bases (I-frames) and diffs (P-frames).

### Base (I-Frame)

A complete snapshot of all B-tree roots and the index state. Everything needed to reconstruct the full filesystem state without any diffs.

A base does NOT copy all data chunks — it captures the B-tree structure (which nodes exist, what they point to). Data chunks are shared across versions via content addressing.

### Diff (P-Frame)

A minimal delta from the previous state:
- Chunks added (new chunk hashes)
- Chunk headers modified (next/prev pointer changes)
- Index entries added/removed/modified in B-tree nodes
- B-tree nodes split/merged

A diff for a small change (one file updated) is tiny — a few B-tree node changes.

### Version Timeline

```
Base₁ → diff → diff → diff → diff → Base₂ → diff → diff → diff → Base₃
  v1     v2     v3     v4     v5      v6      v7     v8     v9      v10
```

### Restoring a Version

1. Find the nearest base at or before the target version
2. Apply diffs sequentially until the target version
3. Done

### When to Create a New Base

- After N diffs (configurable threshold)
- When cumulative diff size exceeds a percentage of the base size
- On explicit user request
- The engine can decide automatically (like a video encoder choosing I-frame intervals)

### Storage Savings

1000 versions with a base every 100 = 10 bases + 990 diffs. If each version changes 0.01% of the data, the diffs are minuscule versus storing 1000 full snapshots.

### Replication Efficiency

Syncing a new node:
1. Send the latest base
2. Send the diffs since that base
3. Node is caught up

No need to replay the entire history.

---

## Repair and Recovery

The linked chunk structure makes the database highly repairable:

- **Corrupt chunk detected:** BLAKE3 hash mismatch on read. Skip the chunk, continue following links from the previous chunk. You lose one chunk's worth of data, not the entire file.
- **Broken link:** A chunk's `next` points to a hash that doesn't exist. The file is truncated at that point but everything before it is recoverable.
- **Corrupt B-tree node:** Re-scan the leaf level (linked list) to reconstruct the branch nodes. The leaves contain all the data; branches are just navigation shortcuts.
- **Lost root:** Scan all chunks, identify B-tree leaves by structure, reconstruct the tree bottom-up.
- **Version recovery:** If the latest state is corrupt, walk backward through diffs to find the last clean base. Restore from there.

Corruption is always **local**. A bad chunk doesn't cascade. The linked structure means you can always find the next good chunk and keep going.

---

## Dot-Path Conventions

System paths use dot-prefix (Unix hidden file convention):

| Path | Purpose | Default Permissions |
|---|---|---|
| `.config` | Path configuration (parsers, validators, permissions) | admin: full, users: read |
| `.indexes` | Search index data | engine: full, admin: full, users: read |
| `.system` | System-level data | admin: full, users: none |

These are just regular files/directories. The engine gives them special meaning by convention, not by enforcement. Permissions control access, not naming rules.

---

## Mandatory Document Metadata

Every file stored has engine-managed metadata:

| Field | Type | Description |
|---|---|---|
| `document_id` | UUID v4 | Unique identifier |
| `created_at` | Timestamp | When created |
| `updated_at` | Timestamp | When last modified |

No soft-delete. Delete is real delete. Recovery is via version restore.

---

## Summary of Data Structures

| Concept | Implementation |
|---|---|
| Chunk | Header (next/prev) + Data (hashed). The one primitive. |
| File | Linked list of chunks. |
| Directory | Per-directory B-tree. Nodes are chunks. Leaves contain index entries. |
| Index Entry | Name + type (File/Directory/HardLink) + first chunk hash. |
| Hard Link | Index entry pointing to another file's first chunk. |
| Version Base | Complete B-tree root snapshot (I-frame). |
| Version Diff | Minimal delta of B-tree changes (P-frame). |
| Permissions | `crudlify` tri-state flags on group→path links. Resolved per segment during traversal. |
| Configuration | Stored at `.config` files within directories. Inherited downward. |

---

## Relationship to Existing Code

The current Phase 1-4 implementation uses redb as the storage backend with a simpler chunk store model. The architecture described here is the **target state**. Migration path:

1. Current: redb-backed key-value storage with content-addressed chunks (Phase 4.1)
2. Target: linked chunk lists with B-tree directory indexes, base+diff versioning
3. The `ChunkStorage` trait provides the abstraction boundary — the physical chunk storage can be swapped without changing the layers above
4. The B-tree and linked-list structures will be built on top of the existing chunk primitives

The existing 380 tests continue to validate the current implementation while the new architecture is built alongside it.
