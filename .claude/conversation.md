# AeorDB — Storage Engine Design Conversation

This document tracks the evolving design conversation. Rounds are marked clearly. **Read from the bottom up for the latest thinking.** The canonical plan is always at `bot-docs/plan/custom-storage-engine.md`.

---
---

# ROUND 1 (SUPERSEDED)

*This round explored the initial Bitcask-inspired design. Key concepts introduced: append-only WAL, in-memory index, entity types. Several ideas from this round were revised or replaced in later rounds.*

## Initial Concepts (kept)
- Append-only data file IS the WAL
- Everything is an entry with a common header
- BLAKE3 for all integrity (no CRC)
- Domain-prefixed hashing to prevent collisions between types
- Single .aeor file

<!-- 
Claude, let's default to `.aeordb`... I already have other _aeor_ products, so I want to be a bit more specific here.
 -->

## Ideas from Round 1 that were REVISED
- ~~Full in-memory index for all chunks~~ → replaced by NVT + on-disk KV block (Round 2)
- ~~Two-tier index (Tier 1 in-memory, Tier 2 on-disk)~~ → replaced by NVT + KV block (Round 2)
- ~~CRC32 checksums~~ → replaced by BLAKE3 everywhere
- ~~group_id on chunks for recovery~~ → replaced by FileRecord-based recovery (Round 2). Chunks don't "belong" to files — files reference chunks. Recovery scans FileRecords, not chunk group_ids.
- ~~Compaction~~ → not needed with versioning (nothing is ever truly dead)
- ~~Sorted chunk index file~~ → replaced by NVT + KV block
- ~~Bloom filter for chunk lookup~~ → replaced by NVT

---
---

# ROUND 2 — NVT + KV Store + Void Management

*This round solved the "how to find chunks at billion-scale" problem and established the three-layer architecture.*

## Key Decisions

### Three Layers
```
Layer A: NVT + KV Store (master index — in memory + front of file)
Layer B: DirectoryIndex (filesystem tree — Merkle tree of DirectoryRecords)
Layer C: FileRecords + Chunks (raw data — the foundation of truth)
```

### NVT (Normalized Vector Table)
- In-memory structure mapping hash scalars to KV block buckets
- `scalar = first_8_bytes_of_hash_as_u64 / u64::MAX` → [0.0, 1.0]
- Each bucket points to a range in the on-disk KV block
- Self-correcting: scans update bucket boundaries for better precision
- Grows by doubling bucket count (threshold determined by stress testing)
- Tiny footprint: 16 KB at 1024 buckets, 256 MB at 16M buckets (1 billion chunks)

### KV Block (on disk, front of file)
- Sorted array of `(hash: [u8;32], offset: u64)` = 40 bytes per entry
- Lives at front of file, right after header + NVT
- Grows by bulk-relocating entities in its way to end of file
- Relocation is crash-safe: copy first, update offsets second, claim space last

<!-- 
FYI Claude: As I said, I have already built an "NVT + KVS" many years ago (somewhere around 2012). One of the nice features of this technology is that the NVT _also_ assists with sorting. It might not _perfectly_ place each item, but it gets each item _really close_ to where it is supposed to be in the map. Just something to think about.
 -->

### Void Management
- Voids are free space from relocations, tracked as Void entities in the KV store
- Deterministic hashes: `BLAKE3("::aeordb:void:{size}")` → list of offsets
- New writes check for fitting voids before appending to end of file
- Best-fit with splitting: leftover space becomes a new smaller void
- Minimum useful void: 89 bytes (entry header size)

<!-- 
Don't we need more than _just_ an entry header? Wouldn't an entry _at least have some data_ attached to it?

Another thought I have: We need to check if we are already at the "end" of the file before we create a Void. If we are, discarding wouldn't create a Void (i.e. when we create and discard a temporary buffer KVS + NVT during a resize, if that is _already the last thing on the end of the file_, then DO NOT create a void). i.e. simply truncate the file if the "Void" would be the last thing in the file.
 -->

### KV Resize Mode
- When KV block needs to grow: enter resize mode
<!-- Resize mode should be a flag we store on the DB header... in case of a power outage, we would need to know that we _were in_ resize mode during the failure -->
- Spin up temporary buffer KVS+NVT for writes during resize
- Bulk-relocate entities blocking growth (single read + single write)
- Expand KV block into freed space
- Merge buffer into primary, discard buffer, mark as void
- Exit resize mode

<!-- 
We should also place the offset to the buffer KVS+NVT into the DB header... in case of crash, we can relocate it, and properly cleanup.
 -->

## Entity Types (finalized in Round 2)

| Type | Key | Purpose |
|---|---|---|
| Chunk (0x01) | BLAKE3("chunk:" + data) | Raw file data |
| FileRecord (0x02) | BLAKE3("file:" + path) | File metadata + ordered chunk hash list |
| DeletionRecord (0x03) | BLAKE3("del:" + path + ":" + ts) | Marks file deletion for version history |
| Snapshot (0x04) | BLAKE3("snap:" + name) | Serialized directory index (fast startup) |
| Void (0x05) | BLAKE3("::aeordb:void:" + size) | Tracks reusable free space |
| KVEntry (0x06) | varies | KV store internal |

## Recovery (finalized in Round 2)
- FileRecords contain ordered chunk hash lists → files are self-describing
- DeletionRecords enable full version history reconstruction
- Entity-by-entity scan (jump via `total_length` field, not byte-by-byte)
<!-- 
Hopefully our Voids will catch everything... but we already discussed that we might end up with "small gaps" that we "ignore". This is going to be REALLY BAD if we are skipping through the DB, and we hit an unlabeled "gap". We might want to ALWAYS write a void in ANY empty space, even if the void is incredibly small, simply to allow us to continue to "skip" through the database. 
 -->
- KVS and DirectoryIndex are rebuildable; chunks and FileRecords are truth

## Chunk Lookup Path (the actual current design)
```
1. Hash the key (or already have a chunk hash)
2. Compute scalar from hash → NVT bucket lookup (in memory)
3. Bucket tells you: go to offset X in KV block, scan Y entries
4. Seek to KV block offset (on disk), scan Y entries (each 40 bytes)
5. Find matching hash → get entity's file offset
6. Seek to file offset → read entity
```
Two disk seeks per lookup. NVT is always in memory.

---
---

# ROUND 3 — DirectoryIndex Design (CURRENT)

*This round designs how directories and file listings work.*

## The Problem

The KV store maps hashes to offsets — great for "find chunk X" or "find FileRecord for path Y." But it can't answer "list all files under `/myapp/users/`" — that's a prefix query on paths, and hashes don't support prefix queries.

## DirectoryRecords — A Merkle Tree

Each directory is a single entity containing a list of its children:

```
DirectoryRecord entity:
  Key:   BLAKE3("dir:" + path)
  Value: serialized list of ChildEntry structs
```

<!-- 
You probably already know this: we need to make sure we "normalize" the path, double-slashes need to be collapsed into single forward slashes, we need to always have a forward slash at the end of the path (my preference)... or not. I want paths to be just like in Unix: forward slashes, sequential forward slashes get collapsed, case sensitive, root starts with /
 -->

Each child:
```
ChildEntry:
  <!-- 
  Claude, I would like to keep variable length data AT THE END of entities... please do this for all our entities: all static header data comes first, followed by variable length data.
   -->
  name:         String        // just the filename, not full path
  <!-- 
  What is a "string" type Claude? Is this short-hand for "name_length"/"name"?
  Or... are you defining the Rust interface, and NOT the file interface? I think you are...
   -->
  entry_type:   u8            // File=1, Directory=2
  hash:         [u8; 32]      // hash of child's entity (FileRecord or child DirectoryRecord)
  total_size:   u64           // file size (0 for directories)
  content_type: Option<String>
  <!-- 
  I like the idea of storing the mime-type. Do we have a good mime-type library for accurately detecting the mime-type of any given file? I don't want to go based just off extension alone... I want something like this: https://github.com/sindresorhus/file-type

  Let's create our own Rust create from this JS library if a good Rust alternative doesn't currently exist.
   -->
  created_at:   i64           // UTC ms
  updated_at:   i64           // UTC ms
```

## How It Works

**Listing `/myapp/users/`:**
1. Compute `BLAKE3("dir:/myapp/users")`
2. KV store lookup → offset of DirectoryRecord
3. Read DirectoryRecord → child list
4. Return children (metadata included — no FileRecord reads needed for `ls`)

<!-- 
HOW do you GET here Claude? Is the idea to `BLAKE3("dir:/myapp/")` _first_ to find `users`?

i.e. to know what is in the filesystem, we first have to list /, and then myapp, and then users?

(I am asking "What do we do" as a user who knows nothing about the fileystem... except that root is at /)
 -->

**Storing a file at `/myapp/users/alice.json`:**
1. Write chunks
2. Write FileRecord
3. Update DirectoryRecord for `/myapp/users/` (add/update alice.json child entry)
4. Propagate up: update `/myapp/` DirectoryRecord (updated hash for `users/` child)
5. Propagate up: update `/` DirectoryRecord
6. Update HEAD in header → new root DirectoryRecord hash

<!-- 
This makes sense... hence versioning. I like it.

We probably need to consider a "BEGIN"/"COMMIT" mechanism for the database, so that if we write 1000 files, we don't end up with 1000 versions of the filesystem. This is something we can consider later though (probably?).
 -->

## Versioning = Saving the Root Hash

The root DirectoryRecord hash IS the entire database state. `HEAD` points to it.

- **Create version:** save the current HEAD hash with a name
- **Restore version:** set HEAD to the saved hash
- **Old DirectoryRecords stay in the file** (append-only) — old versions reference them

This is exactly Git's model: commit → tree → blobs.

<!-- 
I like it. Just not sure if we want to repeat this operation per-file write. We might want to consider a "COMMIT" operation.
 -->

## In-Memory Strategy

**On-demand with LRU caching (Option B):**
- Root DirectoryRecord always in memory
- Child directories loaded when accessed, cached
- LRU eviction for large deployments
- Scales from tiny to massive without configuration

<!-- 
Amen!
 -->

## Questions for Wyatt

1. Does the "propagate up to root" model feel right? Every file write touches every ancestor directory.
<!-- 
Yes, but might be a little insane for 1000 file writes. We should talk about a deliberate "COMMIT" mechanism (which could be automatic on connection-close for example).
 -->
2. For very large directories (millions of entries), split now or optimize later?
<!-- 
Hhmmmm... this is a very legitimate concern... being a database, we need to _expect_ that we will end up with millions or billions of entries for a directory. Let's put some thought into this right now Claude. What are your thoughts for optimizing this problem?

We could have directories simply be FileRecords, where the content is the list of files... this would then cause the entire system to flow under the optimizations (and recovery) we already have planned. A HUGE directory would be streamable, would have chunks allocated across the DB, and would be more efficient for large data.

Thoughts?

The snapshot would _still_ just be the hash of the "file" that was the directory list.
 -->
3. On-demand tree navigation (Option B) vs full in-memory tree (Option C)?
<!-- 
I think we need to keep commonly accessed entries in memory, evict uncommonly accessed entries, and lazy-load on-demand.
 -->
4. Should DirectoryRecord children include full metadata (fast `ls`) or just hashes (smaller records)?
<!-- 
I'd like metadata: file type, file size, etc... for quick listings (and minimal DB crawling). However... this means if we change a file, we HAVE to change all directory entries... though I guess we already have to do this for every write (which I am now seriously considering... we might need a "COMMIT" mechanism to deliberately take a snapshot).
 -->

---
---

# ROUND 4 — Resolved Decisions + Transactions + Directories-as-Files

*Incorporating all of Wyatt's Round 3 feedback.*

## Resolved from Round 3

| Decision | Resolution |
|---|---|
| File extension | `.aeordb` (not `.aeor` — other aeor products exist) |
| Minimum void | Must be large enough to hold at least a void entry header. Every gap, no matter how small, gets a void entry so entity-by-entity scan always works. |
| Void at EOF | Don't create a void — just truncate the file. Only create voids for gaps in the MIDDLE. |
| Resize mode flag | Store in DB header. On restart, if set, complete or roll back the resize. Also store buffer KVS+NVT offset in header. |
<!-- 
Let's never roll-back, but proceed with the resize. If a resize was underway, then _it is needed_.
 -->
| Path normalization | Unix-style: forward slashes, collapse doubles, case sensitive, root is `/`, normalize on write. |
| On-disk string format | Always `length: u16` + `data: [u8; length]`. No ambiguity. |
| Fixed before variable | All entities: fixed-size fields first, variable-length fields last. |
| MIME detection | Detect from content, not just extension. Research Rust crates (e.g., `infer`, `tree_magic_mini`). |
| Directory listing | Direct hash lookup: `BLAKE3("dir:/myapp/users/")` → KV store → done. No root traversal needed. |
| In-memory strategy | On-demand + LRU cache. Root always loaded. Children lazy-loaded. Eviction for large deployments. |
| Metadata in listings | Yes — include file type, size, timestamps in directory entries. Fast `ls` without FileRecord lookups. |
| NVT assists sorting | Noted from Wyatt's prior implementation experience — NVT naturally places items near their sorted position. |

## Directories ARE FileRecords

**Key insight from Wyatt:** A directory is just a FileRecord whose content happens to be a serialized list of child entries. No special entity type needed.

```
A directory at /myapp/users/:
  Entity type:   FileRecord (0x02)
  Key:           BLAKE3("file:/myapp/users/")
  Content-Type:  "application/x-aeordb-directory"
  Value:         serialized list of ChildEntry structs
```

### Why This Is Better

- **Large directories get chunked automatically** — a directory with 1 million entries gets split into 256 KB chunks just like any other file
- **Large directories stream on read** — listing a huge directory doesn't load it all into memory
- **Recovery treats directories like files** — same entity-by-entity scan, same reconstruction logic
- **Fewer entity types** — we might drop DirectoryRecord (0x03) entirely and reuse FileRecord

### Revised Entity Types

| Type | Key | Purpose |
|---|---|---|
| Chunk (0x01) | BLAKE3("chunk:" + data) | Raw data blocks |
| FileRecord (0x02) | BLAKE3("file:" + path) | Files AND directories (distinguished by content_type) |
| DeletionRecord (0x03) | BLAKE3("del:" + path + ":" + ts) | Marks deletion for version history |
| Snapshot (0x04) | BLAKE3("snap:" + name) | Serialized HEAD hash + metadata (fast startup) |
| Void (0x05) | BLAKE3("::aeordb:void:" + size) | Tracks reusable free space |

Down to 5 entity types. KVEntry (0x06) was unnecessary — the KV block is a binary format, not individual entities.

### ChildEntry On-Disk Format (fixed fields first, variable last)

```
ChildEntry (on disk):
  entry_type:       u8              // File=1, Directory=2
  hash:             [u8; 32]        // hash of child's FileRecord
  total_size:       u64             // file size (0 for directories)
  created_at:       i64             // UTC ms
  updated_at:       i64             // UTC ms
  name_length:      u16             // child name length
  name:             [u8; name_length]  // just the filename, not full path
  content_type_len: u16             // MIME type length (0 if none)
  content_type:     [u8; content_type_len]  // MIME type
```

Fixed fields: 1 + 32 + 8 + 8 + 8 = 57 bytes. Then variable fields.

## Write Transactions — BEGIN / COMMIT

### The Problem

Writing one file propagates DirectoryRecords to the root, creating a new "version." Writing 1000 files creates 1000 intermediate root versions — wasteful.

### The Solution: Explicit Transactions

```
BEGIN TRANSACTION
  write file A
  write file B
  write file C
  ...
COMMIT
```

During a transaction:
- Chunks and FileRecords are written immediately (append to file, fsync)
- DirectoryRecord propagation is DEFERRED — accumulated in memory
- On COMMIT: propagate all directory changes at once, write one new root, update HEAD
- On ABORT (or crash): chunks and FileRecords are on disk but unreferenced. They're orphans until the next commit creates a root that references them. (Garbage collection could clean them up, but with versioning they're essentially harmless.)

<!-- 
Claude... one problem we have is that HTTP is stateless... how do we "create a transaction" in a stateless system?

I am wondering if instead of a begin/commit, we simply have a "snapshot" command. This would save the state, and return the version hash.

This could then be run automatically by the system every so often (admin configured)... like maybe every hour (if things have changed), or something?

If we _really do want_ a transaction type "begin", then it might go something like this:

1. Save a snapshot (this becomes the base/"begin" of the transaction)
2. Create a "named version" that is a fork of this base version
3. The HTTP requests are now include a "named version" to place the new files into
4. This "named version" now points to the "fork" of the original hash
5. When a final "snapshot" command is sent for the "fork"/named version, at that point we generate a final hash (like any other snapshot), and that becomes the "commit".

Thoughts?

Forks + deliberate snapshots would essentially fully replace SQL BEGIN/COMMIT transactions.

Important note!: This means that we need to allow callers to _specify a version hash_ for any given operation. Whatever they do now updates the _version specified_ (or a fork of it).

This should be fully possible. Instead of finding the "HEAD" "directory index" hash and work on that, we simply work off the version the user supplied instead.
 -->

### Auto-Commit

If no explicit transaction: auto-commit after each write (current behavior). This preserves the simple single-write case.

If a client connection closes without COMMIT: auto-commit what's been written, or auto-abort (configurable?).

<!-- 
I think this should be admin configurable. Where and when we take snapshots should be decided by the admin. I am certainly not against a snapshot per-file-stored... but for _some_ scenarios this could be _quite bad_. These types of things need to be fully configurable.
 -->

### HTTP Mapping

```
POST /transaction/begin          → returns transaction_id
PUT  /fs/path (with tx header)   → writes within the transaction
POST /transaction/commit         → propagates + updates HEAD
POST /transaction/abort          → discards pending directory changes
```

Or simpler: batch write endpoint that accepts multiple files in one request and auto-commits.

<!-- 
The batch write endpoint is a very interesting idea... I actually really like it, and think we should support this _as an alternative_ to transactions.

What you have designed here isn't really that much different from what I said above. Let's see if we can put our heads together and come up with a solution that is both of our ideas combined.
 -->

## Questions for Wyatt

1. **Auto-commit on connection close** — commit or abort? I'm leaning commit (don't lose work), but abort is safer (don't commit partial state).
<!-- 
We need to have this configurable, and leave it up to the admin.
 -->
2. **Orphaned chunks from aborted transactions** — leave them (harmless, versioning keeps everything), or clean up?
<!-- 
Clean them up... the user my never return to finish them off. We don't want dead/orphaned chunks simply lying around.

However, we need to be careful about this, as this operation needs to know "how many files own me?"... we can NEVER
remove a chunk if even one file _used to_ reference it.

Thinking of a system to track "who owns me?" is an interesting concern all of its own...
 -->
3. **Directories-as-FileRecords** — you suggested this. Just confirming: we drop the separate DirectoryRecord entity type (0x03) and unify under FileRecord?
<!-- 
I like the idea of using a FileRecord... I don't like the idea of dropping the type.

What if a DirectoryIndex _is_ a FileRecord, with the only distinction being the type?

I think it is valid and important to continue to have a different type (even if it is the same structure).
 -->
4. **MIME detection crate** — should I research Rust options now, or defer to implementation time?
<!-- 
I'd like to research now. I don't like to leave anything up to "guessing" when it comes to implementation bots.
 -->

---
---

# ROUND 5 — Forks, Snapshots, and Resolved Decisions

## Resolved from Round 4

| Decision | Resolution |
|---|---|
| Resize recovery | Never roll back. Always complete the resize on crash recovery. |
| DirectoryIndex type | Keeps its own type ID (0x03), same structure as FileRecord. Distinct type for explicit recovery/scanning. |
| Auto-commit | Admin configurable. Not hardcoded. |
| Orphaned chunks | Clean up, but only if NO version/fork references them. Needs reference tracking (future design). |
| Snapshots | Admin-configured schedule (every N writes, every N minutes, on explicit command). NOT automatic per-file. |
| Batch writes | Supported alongside transactions as a simpler alternative. |
| MIME detection | `file-format` crate (v0.29.0) — 200+ formats, zero deps, works on `&[u8]` |
| Entity versioning | Every entry header has `entry_version: u8`. Engine maps version → parser. Old entries always readable. |
| Hash algorithm | Stored as `hash_algo: u16` enum in entry header AND file header. Hash length is dynamic based on algorithm. Default: BLAKE3_256 (32 bytes). |
| Dynamic hash lengths | All hash fields use `hash_length(hash_algo)` instead of hardcoded 32. KV entries, NVT, FileRecord chunk lists — all adapt. |
| NVT/KVS versioning | Both have their own version byte. Engine selects correct reader per version. |
| File header | Expanded to 256 bytes. Includes: header_version, hash_algo, resize flags, buffer KVS/NVT offsets. |
| MIME detection | Researching Rust crates now (agent running). |

## Versioning Model: Forks + Snapshots (replaces BEGIN/COMMIT)

### The Core Idea

HTTP is stateless. Traditional BEGIN/COMMIT requires session state. Instead: **forks and snapshots**, inspired by Git branches.

```
Every operation specifies: "which version am I working against?"
Default: HEAD (the latest committed state)
Optional: any version hash or named fork
```

### How It Works

**Simple case (no explicit versioning):**
```
PUT /fs/myapp/users/alice.json
→ Writes against HEAD
→ System takes a snapshot per admin-configured schedule
```

**Transactional case (fork-based):**
```
1. POST /version/snapshot → returns current_hash (base version)
2. POST /version/fork?base=current_hash&name=my-batch → creates a named fork
<!--
Claude, let's have this be: `base=HEAD` 
 -->
3. PUT /fs/myapp/users/alice.json (header: X-Version: my-batch) → writes to fork
4. PUT /fs/myapp/users/bob.json (header: X-Version: my-batch) → writes to fork
5. POST /version/snapshot?fork=my-batch → takes snapshot of fork, returns new_hash
6. POST /version/promote?hash=new_hash → makes this the new HEAD (the "commit")
```

<!--
I absolutely love this! Good work! 
 -->

**Batch case (simpler):**
```
POST /fs/_batch
Content-Type: multipart/mixed

--boundary
PUT /myapp/users/alice.json
<data>
--boundary
PUT /myapp/users/bob.json
<data>
--boundary--
→ All writes applied, single snapshot taken at the end
```

### What This Enables

- **Multiple concurrent writers on different forks** — no conflicts
<!--
What about the rare case where we just happen to be writing the same chunk at the same time? Though I guess you'd have to already add the entry to the KVS? We should make sure we always add an entry to the KVS... maybe in a "pending" state? 
 -->
- **Atomic multi-file operations** — write to a fork, snapshot when done
- **Review before commit** — fork exists, you can read from it, verify, then promote to HEAD
- **Rollback** — just don't promote. The fork is abandoned (chunks cleaned up eventually).
- **Branching** — long-lived forks for A/B testing, staging environments, etc.
- **Stateless HTTP** — every request carries its version context in a header

<!--
Love it! 
 -->

### Implementation

A "fork" is just a separate HEAD pointer. When you write to a fork:
1. The fork has its own root DirectoryIndex hash
2. Writes update the fork's directory tree (propagate up to the fork's root, not HEAD's root)
3. Chunks are shared across forks (content-addressed dedup)
4. Snapshot = save the fork's current root hash with a name

### Data Model

```
Stored in KV store:
  "::aeordb:head" → hash of current HEAD root DirectoryIndex
  "::aeordb:fork:my-batch" → hash of fork's root DirectoryIndex
  "::aeordb:version:v1.0" → hash of named version snapshot
  "::aeordb:version:2026-03-30T12:00:00Z" → auto-snapshot hash
```

All just KV entries. No special machinery.

<!--
We might also have to add a "flags" here along with a hash. I just decided I also want this to have a "type".

One u8 = lower 4 bits = type enum, upper 4 bits = flags, one of those flags is "pending".
 -->

## Revised Entity Types (Final)

| Type ID | Name | Purpose |
|---|---|---|
| 0x01 | Chunk | Raw data blocks |
| 0x02 | FileRecord | File metadata + ordered chunk hash list |
| 0x03 | DirectoryIndex | Same structure as FileRecord, but type-tagged as directory. Content is serialized child entries. |
| 0x04 | DeletionRecord | Marks file/directory deletion for version history |
| 0x05 | Snapshot | Stores a named version hash + metadata |
| 0x06 | Void | Tracks reusable free space by size |

Six types. Clean separation. Type tag enables recovery scanning without parsing values.

## Questions for Wyatt

1. **Fork promotion** — when promoting a fork to HEAD, should it be a fast-forward (just move the HEAD pointer) or a merge (combine fork's changes with any HEAD changes made since the fork was created)?
<!--
No, no merges. We will need to support merges at some point, but that will be future, and that will also be a different HTTP action. 
 -->
2. **Fork cleanup** — abandoned forks (never promoted, never snapshotted). Auto-delete after N days? Manual only?
<!--
Well, first, this is a good case for our entity "flags". We should have a "deleted" flag. We can then have a cron-type task that crawls the DB, repairs crap, and cleans up garbage. So the explicit "rollback" would cause orphaned chunks from being labeled "dropped", and they would be cleaned up later... unless they got reclaimed, before they were dropped. We still have to figure out how to quickly get a count of how many "owners" any given chunk has. Maybe ref counting? Do you have a better idea? 
 -->
3. **Version naming** — should auto-snapshots use timestamps? Sequential IDs? Both?
<!--
How about `auto-{timestampt}`? I don't really want to track a counter... 
 -->
4. **Does the fork model feel right?** It's more complex than simple BEGIN/COMMIT but it's stateless and supports concurrent writers naturally.

<!--
I absolutely love it! It will mean we have to get into more tools and operations that you can apply to the database... but that was coming anyhow, and doesn't bother me. 
 -->

---

## Resolved: MIME Detection

**Crate: `file-format` (v0.29.0)**
- 200+ formats, zero deps (base config), pure Rust
- Works on `&[u8]` slices (our use case)
- Handles text formats (HTML, XML, SVG, JSON) that magic-byte-only libraries miss
- MIT OR Apache-2.0
- Very actively maintained (last updated March 2026)
- Handles images, video, audio, documents, archives, web formats

---
---

# ROUND 6 — Resolved: Forks, Flags, Ref Counting

## Resolved from Round 5

| Decision | Resolution |
|---|---|
| Fork model | Approved. Forks + snapshots replace transactions. Stateless, concurrent. |
| Fork promotion | Fast-forward only (move HEAD pointer). No merges for now — future work. |
| Fork cleanup | "Deleted" flag on KV entries + background cron task cleans up. |
| Auto-snapshot naming | `auto-{timestamp}` (e.g., `auto-2026-03-30T12:00:00Z`). No counter. |
| Concurrent chunk writes | KV entries start in "pending" state. Marked active after fsync+verify. Dedup: if hash already exists, skip write. |
<!-- 
We will need to know the hash ahead of time... and I have future plans to have the client send the data to us pre-hashed and chunked. For now, just know that we will need to know the hash ahead of time.
 -->
| KV entry flags | `u8`: lower 4 bits = type enum, upper 4 bits = flags (pending, deleted, etc.) |

## KV Entry Type+Flags Byte

```
Bits 0-3 (type):
  0x0 = chunk reference
  0x1 = file record reference
  0x2 = directory index reference
  0x3 = deletion record reference
  0x4 = snapshot reference
  0x5 = void reference
  0x6 = head pointer
  0x7 = fork pointer
  0x8 = version pointer
  0x9-0xF = reserved

Bits 4-7 (flags):
  bit 4 = pending (not yet fsynced/verified)
  bit 5 = deleted (marked for cleanup)
  bit 6 = reserved
  bit 7 = reserved
```

So a KV entry is now: `type_flags: u8` + `hash: [u8; N]` + `offset: u64` = 1 + N + 8 bytes per entry.

## Chunk Ownership: Ref Counting + Periodic Tracing

**Primary mechanism: reference counting.**
- Each chunk has a ref count (stored in the KV entry or a separate counter)
- When a FileRecord references a chunk: increment
- When a FileRecord is deleted/superseded: decrement
- When count reaches 0: flag as "dropped" (eligible for cleanup)

**Secondary mechanism: periodic tracing (cron task).**
- Walk all live roots (HEAD + all forks + all named versions)
- Mark all reachable chunks
- Any chunk not marked but not flagged "dropped" = ref count is wrong → fix it
- Any chunk flagged "dropped" but actually reachable = ref count was wrong → unflag it

Tracing reconciles ref counting errors. Ref counting provides fast day-to-day tracking. Together, they're robust.

**Where to store the ref count:**
- Option A: In the KV entry itself (add a u32 field → KV entry grows by 4 bytes)
- Option B: In a separate ref count table (another KV-like structure)
- Option C: In the chunk's entry header on disk (but this requires seeking to the chunk to read/update)

Leaning Option A — ref count in the KV entry. It's always in the hot path (KV lookups already read the entry), no extra seeks.

<!-- 
Let's worry about this in the future. Let's drop this part of the plan for now. Just write a skel "future-plan" that references garbage cleanup, repair, and whatnot as a CRON task.
 -->

## Questions for Wyatt

1. **Ref count in KV entry** — Option A (add u32 to KV entry, 4 extra bytes per entry)? Or separate structure?
<!-- 
Let's drop for a future-plan.
 -->
2. **Cron task frequency** — how often should the tracing reconciliation run? Daily? Weekly? On admin command?
<!-- 
Let's drop for a future-plan.
 -->
3. **Are we ready to start building?** We've resolved: entity format, NVT, KVS, voids, forks, snapshots, directory index, MIME detection, versioning, hash algorithms, ref counting, flags. What's left to design?

---
---

# ROUND 7 — Final Resolutions, Ready to Build

## Final Resolved

| Decision | Resolution |
|---|---|
| Entry magic number | `0x0AE012DB` (4 bytes) — first bytes of every entry. Enables reliable recovery scanning. |
| Ref counting / GC | Deferred to future-plans.md |
| Cron task system | Deferred to future-plans.md |
| Pre-hashed client uploads | Deferred to future-plans.md (client sends hash ahead of time — noted for API design) |
| Merge operations | Deferred to future-plans.md (fast-forward only for now) |

## AGIS Gauntlet Results

All operations verified as designed: store/read chunks, store/read/delete files, directory listings, forks, snapshots, restore, voids, KVS resize, recovery (normal + catastrophic). One gap found (magic number) — resolved.

Test plan: ~60+ tests across 9 spec files covering entry format, append writer, NVT, KVS, voids, file records, directories, versioning, recovery, and integration.

## Status: READY TO BUILD

Future plans captured in `bot-docs/plan/future-plans.md`.

---

*Plan complete. Implementation begins.*

<!-- 
We are getting damn close to building. Have we ::agis.ponder what else we are missing? Have we ::agis.test* what we didn't test, and how we are going to test? Did we apply other AGIS gauntlets?
 -->
