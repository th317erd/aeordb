# Storage Engine

The storage engine is an append-only WAL (write-ahead log) where the log IS the database. Every write appends a new entry. The file only grows (until garbage collection reclaims unreachable entries). This design gives you crash recovery, versioning, and integrity verification as structural properties rather than bolted-on features.

## Entry Format

Every entry on disk shares the same header format:

```
[Entry Header - 31 bytes fixed + hash_length variable]
  magic:            u32    (0x0AE012DB - marks the start of a valid entry)
  entry_version:    u8     (format version, starting at 1)
  entry_type:       u8     (Chunk, FileRecord, DirectoryIndex, etc.)
  flags:            u8     (operational flags)
  hash_algo:        u16    (BLAKE3_256 = 0x0001, SHA256 = 0x0002, etc.)
  compression_algo: u8     (None = 0x00, Zstd = 0x01)
  encryption_algo:  u8     (None = 0x00, reserved for future use)
  key_length:       u32    (length of the key field)
  value_length:     u32    (length of the value field)
  timestamp:        i64    (UTC milliseconds since epoch)
  total_length:     u32    (total bytes including header, for jump-scanning)
  hash:             [u8; N] (integrity hash, N determined by hash_algo)

[Key - key_length bytes]
[Value - value_length bytes]
```

Key properties:
- **`magic`** (0x0AE012DB) enables recovery scanning -- find entry boundaries even in a corrupted file by scanning for magic bytes
- **`total_length`** enables jump-scanning -- skip to the next entry without reading the full key/value
- **`hash`** covers `entry_type + key + value` -- re-hash and compare to detect corruption
- **`entry_version`** enables format evolution -- the engine selects the correct parser based on this byte

For BLAKE3-256 (the default), the hash is 32 bytes, making the full header 63 bytes.

## Content-Addressed Hashing

Every piece of data is identified by its BLAKE3 hash. Hash inputs are prefixed by type (domain separation) to prevent collisions between different entry types:

| Entry Type | Hash Input | Example |
|------------|-----------|---------|
| Chunk | `chunk:` + raw bytes | `BLAKE3("chunk:" + file_bytes)` |
| FileRecord (path key) | `file:` + path | `BLAKE3("file:/users/alice.json")` |
| FileRecord (content key) | `filec:` + serialized record | `BLAKE3("filec:" + record_bytes)` |
| DirectoryIndex (path key) | `dir:` + path | `BLAKE3("dir:/users/")` |
| DirectoryIndex (content key) | `dirc:` + serialized data | `BLAKE3("dirc:" + dir_bytes)` |

The domain prefix ensures that a chunk's raw data can never produce the same hash as a file path, even if the bytes are identical.

## Chunking

Files are split into 256KB chunks for storage. Each chunk is content-addressed independently:

```
Original file (700KB):
  [Chunk 1: 256KB] -> hash_a
  [Chunk 2: 256KB] -> hash_b
  [Chunk 3: 188KB] -> hash_c

FileRecord:
  path: "/docs/report.pdf"
  chunk_hashes: [hash_a, hash_b, hash_c]
  total_size: 700KB
```

Chunking provides:
- **Deduplication**: Two files sharing identical 256KB blocks store those blocks only once
- **Efficient updates**: Modifying 3 bytes of a 10GB file creates one new chunk, not a new copy of the entire file
- **Streaming reads**: Read a file by iterating its chunk hashes and fetching each chunk

## Dual-Key FileRecords

FileRecords are stored at two keys to support both current reads and historical versioning:

1. **Path key** (`file:/path`) -- mutable, always points to the latest version. Used for reads, metadata, indexing, and deletion. O(1) lookup.

2. **Content key** (`filec:` + serialized record) -- immutable, content-addressed. The directory tree's `ChildEntry.hash` points to this key.

When the version manager walks a snapshot's directory tree, it follows `ChildEntry.hash` to the content key, which resolves to the FileRecord as it existed at snapshot time -- not the current version. This is what makes historical reads correct.

Directories use the same pattern: `dir:/path` (mutable) and `dirc:` + data (immutable content key).

## FileRecord Format

```
[FileRecord Value]
  path_length:      u16
  path:             [u8; path_length]     (full file path)
  content_type_len: u16
  content_type:     [u8; content_type_len] (MIME type)
  total_size:       u64                    (file size in bytes)
  created_at:       i64                    (UTC milliseconds)
  updated_at:       i64                    (UTC milliseconds)
  metadata_length:  u32
  metadata:         [u8; metadata_length]  (arbitrary JSON metadata)
  chunk_count:      u32
  chunk_hashes:     [u8; chunk_count * 32] (ordered BLAKE3 hashes)
```

Metadata fields come first so you can read file metadata without skipping past the chunk list. Chunk hashes are the tail of the record for streaming reads.

## Directory Propagation

When a file is stored or deleted, the change propagates up the directory tree:

```
Store /users/alice.json:

1. Store chunks -> [hash_a, hash_b]
2. Store FileRecord at path key + content key
3. Update /users/ DirectoryIndex (new ChildEntry for alice.json)
4. Update / root DirectoryIndex (new ChildEntry for users/)
5. Update HEAD in file header (new root hash)
```

Each directory gets a new content hash because one of its children changed. This chain of updates from leaf to root is what maintains the Merkle tree and makes versioning work.

## Void Management

When garbage collection reclaims an entry, the space becomes a Void -- a marker for reclaimable space. Voids are tracked by size using deterministic hash keys:

```
Key:   BLAKE3("::aeordb:void:262144")
Value: [list of file offsets where 262144-byte voids exist]
```

When a new entry needs to be written, the engine checks for a void of sufficient size before appending to the end of the file. If a void is larger than needed, it is split: the entry occupies the front, and a smaller void is created for the remainder (if the remainder is at least 63 bytes -- the minimum entry header size).

## Compression

Compression is a post-hash transform:

```
Write: raw data -> hash -> compress -> store
Read:  load -> decompress -> verify hash -> return
```

The hash is always computed on the raw uncompressed data. This preserves deduplication (same content = same hash regardless of compression) and integrity verification.

Each entry carries its own `compression_algo` byte, so compressed and uncompressed entries coexist in the same file. Currently, zstd is the only supported compression algorithm.

## fsync Strategy

Not all entries are equally important for durability:

| Data | fsync | Rationale |
|------|-------|-----------|
| Chunks, FileRecords, DeletionRecords | Immediate | The truth -- not rebuildable from other data |
| KV store, NVT, DirectoryIndex, Snapshots | Deferred | Derived data -- can be rebuilt from a full entry scan |

This gives durability where it matters and performance where it doesn't.

## Crash Recovery

The recovery hierarchy, from least to most damage:

| What's Lost | Recovery Method |
|-------------|-----------------|
| Nothing | Read HEAD from KV store, load directory index, ready |
| KV store only | Entry-by-entry scan, rebuild KV store, load latest directory index |
| Directory index only | Scan FileRecords + DeletionRecords, reconstruct from paths + timestamps |
| KV store + directory | Full entry scan, rebuild KV, reconstruct directory |
| Only chunks + FileRecords survive | Full data recovery, version history reconstructed via DeletionRecords |

The magic bytes at the start of every entry enable boundary detection even in partially corrupted files. The `total_length` field in each header enables efficient forward scanning.

## Next Steps

- [Architecture](./architecture.md) -- high-level system overview
- [Versioning](./versioning.md) -- snapshots, forks, and the Merkle tree
