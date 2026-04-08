# Future Plans

Ideas discussed but deferred. Not blocking current implementation.

---

## Server-Side Compilation + In-Database SDK + Schema-as-Code

**What:** Functions are published as raw source code. The server compiles them to WASM. The SDK, models, and functions all live as files in the database filesystem. The database IS the development environment.

### How It Works

Users write Rust source files and push them to the database:

```
PUT /engine/myapp/.functions/models/user.rs     ← schema model
PUT /engine/myapp/.functions/find_users.rs       ← query function
```

On first invocation, the server:
1. Resolves all `mod`/`use` imports from the filesystem
2. Pulls in the SDK (stored at `/.system/sdk/`)
3. Compiles everything together → WASM binary
4. Caches the binary
5. Executes

On subsequent invocations: uses the cached WASM. If source changes: recompile.

### Filesystem Layout

```
/.system/sdk/                      ← SDK lives IN the database
  prelude.rs
  query.rs
  types.rs
  schema.rs
  response.rs

/myapp/.functions/                 ← user code lives IN the database
  models/
    user.rs                        ← schema definition + parser + index config
    product.rs
  find_young_bobs.rs               ← uses models/user.rs + SDK
  generate_report.rs               ← uses multiple models + SDK
```

### Schema-as-Code via Proc Macros

The `#[aeordb_schema]` proc macro on a struct generates:
- **Parser plugin** — knows how to extract fields from raw bytes (JSON, XML, etc.)
- **ScalarConverter registrations** — which converter for each field
- **Index configuration** — automatically configures indexes at the path
- **Typed query builder** — `.name()` returns a `StringFieldQuery`, `.age()` returns a `U64FieldQuery`

```rust
// /myapp/.functions/models/user.rs
use aeordb_sdk::prelude::*;

#[aeordb_schema(parser = "json")]
pub struct User {
  #[index(string, unique)]
  pub email: String,
  #[index(fuzzy)]
  pub name: String,
  #[index(u64)]
  pub age: u64,
}
```

### Query Functions

Functions use the generated typed query builder. Operations accumulate (like Mythix ORM's operation stack) until a terminal method is called:

```rust
// /myapp/.functions/find_young_bobs.rs
mod models;
use models::user::User;

#[query_function]
fn find_young_bobs() -> QueryResult {
  User::query()
    .name().fuzzy("Bob")    // pushed to operation stack
    .age().lt(30)           // pushed to operation stack
    .all()                  // NOW execute: engage indexes, intersect, return
}
```

Terminal methods: `.all()`, `.first()`, `.last()`, `.count()`, `.cursor()` (streaming).

When a terminal method is called, the engine:
1. Looks at configured indexes for the target path
2. For each field operation → uses the matching index
3. Executes index queries → gets candidate sets
4. Intersects/unions candidates
5. Loads matching files
6. Returns results

### Simple Queries (No Compilation)

For simple queries that don't need full Rust, a JSON query API (interpreted, no compiler needed):

```
POST /query
{
  "path": "/myapp/users/",
  "where": {
    "name": { "fuzzy": "Bob" },
    "age": { "lt": 30 }
  }
}
```

Both interfaces use the same underlying index engine.

### Multi-File Compilation

The server resolves `mod` imports by looking up files in the database filesystem. `mod models;` resolves to the `models/` directory at the same path. The SDK at `/.system/sdk/` is implicitly available. All dependencies are in the database.

### Requirements

- Rust compiler (`rustc`) with `wasm32-unknown-unknown` target on the server
- Or: a lighter approach using a Rust-native scripting language (Rhai) for simple cases
- WASM binary cache (keyed by hash of all source files involved)
- Cache invalidation when any source file changes

### What This Enables

- **Schema lives in the database** — versioned, forkable, snapshotable
- **Functions live in the database** — same benefits
- **SDK lives in the database** — upgradeable per-database
- **The database IS the development environment** — no local toolchain needed for simple queries
- **Compile once, run everywhere** — WASM binary is portable across nodes in a cluster

### Open Questions

- [ ] Compilation latency on first invocation (cold start). Cache aggressively to minimize.
- [ ] Rust compiler as a server dependency — heavy. Consider Rhai or Lua for lightweight alternative.
- [ ] Incremental compilation — only recompile what changed.
- [ ] Error reporting — compilation errors need to be surfaced clearly to the user.
- [ ] Sandboxing the compiler itself — compiling arbitrary user code is a security surface.

**Inspiration:** Mythix ORM's proxy-based chainable query engine (JavaScript). The Rust equivalent uses proc macros for compile-time code generation instead of runtime proxies.

### Functions as Endpoints (Programmable Schema)

Published functions are **callable endpoints**. They receive arguments from HTTP request bodies and return results. The database serves them like an API.

**Publishing:**
```
PUT /engine/myapp/.functions/find_users.rs
Body: raw Rust source
```

**Invoking with arguments:**
```
POST /engine/myapp/.functions/find_users/_invoke
{
  "name": "Bob",
  "max_age": 30,
  "limit": 10
}
```

**The function receives typed arguments:**
```rust
#[query_function]
fn find_users(args: Args) -> QueryResult {
  let name: String = args.get("name")?;
  let max_age: u64 = args.get("max_age")?;
  let limit: usize = args.get_or("limit", 100);

  User::query()
    .name().fuzzy(&name)
    .age().lt(max_age)
    .limit(limit)
    .all()
}
```

**Every published function is a custom API endpoint.** The user defines the interface (arguments), the logic (query + computation), and the output (response format). The database serves it over HTTP.

### The Programmable Namespace

Data and code live in the same filesystem, versioned together:

```
/myapp/
  .functions/
    models/
      user.rs              ← schema definition
      product.rs           ← schema definition
    find_users.rs          ← queryable endpoint (accepts arguments)
    create_report.rs       ← computation endpoint
    migrate_data.rs        ← admin operation
    validate_email.rs      ← utility (callable by other functions)
  users/
    alice.json             ← actual data
    bob.json
  products/
    widget.json
```

**Rolling back a version rolls back BOTH the data AND the functions.** The query logic that operated on the data at v1 is preserved with the data from v1. Functions and data are first-class citizens of the same filesystem.

**Functions can call other functions:**
```rust
#[query_function]
fn create_report(args: Args) -> QueryResult {
  let users = invoke("find_users", json!({ "name": args.get("name")? }))?;
  let products = invoke("list_products", json!({ "category": "widgets" }))?;
  // ... compute report from users + products ...
  Response::json(200, &report)
}
```

### What This Makes Possible

- **Custom APIs** — every function is an endpoint. No API server needed.
- **Parameterized queries** — same function, different arguments, different results.
- **Composable logic** — functions call functions. Build complex operations from simple ones.
- **Versioned logic** — roll back data + code together. Test against historical state.
- **Forkable logic** — fork the database, modify the functions, test, promote.
- **Self-documenting** — the function source IS the API documentation. It's right there in the filesystem.

---

## Chunk Ownership & Garbage Collection

**What:** Track how many FileRecords/versions reference each chunk. Clean up unreferenced chunks.

**Ideas discussed:**
- Reference counting in KV entries (u32 per entry)
- Periodic tracing reconciliation (walk all roots, mark reachable, reconcile counts)
- "Dropped" flag on chunks eligible for cleanup

**Why deferred:** Requires careful design to handle concurrent forks/versions. With versioning, chunks are rarely truly orphaned. No data loss risk from deferring — orphaned chunks just occupy space.

---

## Cron/Background Task System

**What:** A scheduled task runner for database maintenance.

**Tasks it would handle:**
- Garbage collection (unreferenced chunk cleanup)
- Integrity verification (walk entities, verify hashes)
- NVT optimization (resize if scan lengths are too high)
- Void consolidation (merge adjacent small voids)
- Stale fork cleanup (delete abandoned forks after configurable period)
- Ref count reconciliation (tracing verification)
- Auto-snapshots (on admin-configured schedule)

**Why deferred:** The database works correctly without it. These are optimization and hygiene operations.

---

## Pre-Hashed & Pre-Chunked Client Uploads

**What:** Allow clients to send data already split into chunks with pre-computed hashes.

**Benefits:**
- Client does the CPU-intensive hashing
- Server just verifies and stores
- Dedup check before upload (client sends hash list, server says "I already have these, only send the new ones")
- Dramatically reduces upload bandwidth for incremental backups

**API concept:**
```
POST /fs/myapp/data/bigfile.bin/_chunked
Content-Type: application/x-aeordb-chunked

{
  "chunk_size": 262144,
  "hash_algo": "blake3_256",
  "chunks": [
    { "hash": "abc123...", "data": "<base64>" },
    { "hash": "def456...", "data": null },  // null = server already has it
    ...
  ]
}
```

**Why deferred:** Requires client SDK support. Current simple PUT works fine for now.

---

## Merge Operations (Fork Merging)

**What:** Combine changes from two forks, handling conflicts.

**Discussed:** Currently fork promotion is fast-forward only (just move HEAD pointer). True merging (combining divergent changes) requires conflict detection and resolution.

**Why deferred:** Fast-forward covers the primary use case (batch writes). Merging is complex and needs careful design (what happens when both forks modify the same file?).

---

## Concurrent Parallel Writers (Coordinator Pattern)

**What:** Single coordinator reserves layout, multiple worker threads fill chunk data in parallel.

**Discussed:** The coordinator writes entry headers with reserved space. Workers backfill chunk data independently. Coordinator fsyncs and updates KVS.

**Why deferred:** Single-writer is correct and sufficient. Parallel writes are an optimization for high-throughput workloads.

---

## Large Directory Optimization

**What:** Directories with millions of entries are currently single FileRecords that get chunked. May need additional optimization for very large directories.

**Ideas:**
- B-tree-structured directory listings (sorted sub-chunks for binary search within directory)
- Indexed child lookups (instead of scanning the full child list)

**Why deferred:** Chunked FileRecords handle large directories already. Optimization for extreme scale.

---

## File Defragmentation

**What:** Over time, void fragmentation accumulates (many small voids). A defrag operation would rewrite the file to eliminate voids and pack entities contiguously.

**Why deferred:** Voids are tracked and reused. Fragmentation is slow-growing. Defrag is an admin-triggered maintenance operation.

---

## Encryption, Vaults, and Zero-Knowledge Multi-User Storage

**What:** Full encryption system enabling secure multi-user storage where nodes can't read the data they store.

### Core Principle: Hash First, Then Encrypt

```
Write path:
  1. Hash the raw plaintext data → chunk hash (BLAKE3)
  2. Check KV store for dedup (hash-based, works on plaintext hashes)
  3. Parse and index the plaintext (parsers + indexers run on raw data)
  4. Encrypt the plaintext → ciphertext
  5. Store ciphertext on disk at the chunk hash address

Read path:
  1. Look up chunk hash in KV → get offset
  2. Read ciphertext from disk
  3. Decrypt with user's key → plaintext
  4. Verify: BLAKE3(plaintext) == chunk hash → integrity confirmed
```

**Key insight:** The hash is on the plaintext, not the ciphertext. This preserves deduplication — two users storing the same file produce the same hash, so the chunk is stored once. The hash doesn't leak content (BLAKE3 is cryptographic, irreversible).

### Processing Pipeline (Serial, Ordered)

Encryption forces a specific order of operations because some steps need plaintext access:

```
1. Receive raw data (plaintext)
2. Hash (needs plaintext) → chunk hash for addressing + dedup
3. Parse (needs plaintext) → extract fields via parser plugins
4. Index (needs parsed fields) → build/update indexes
5. Encrypt (plaintext → ciphertext) → prepare for storage
6. Store (ciphertext) → append to engine file
```

Steps 2-4 MUST happen before step 5. This means the write pipeline is serial for encrypted data — you can't parallelize parsing/indexing with storage because encryption happens in between.

### Vaults (Multi-User Key Sharing)

A **vault** is a group of users who share access to a set of encrypted data.

```
Vault: "engineering-team"
  Members: [alice, bob, carol]
  Vault key: K_vault (symmetric, e.g., AES-256-GCM)

  Each member has:
    - Their identity key pair (from ~/.config/aeordb/identity)
    - The vault key K_vault, encrypted with their public key

  Stored in the vault record:
    encrypted_vault_key_for_alice: encrypt(K_vault, alice_public_key)
    encrypted_vault_key_for_bob: encrypt(K_vault, bob_public_key)
    encrypted_vault_key_for_carol: encrypt(K_vault, carol_public_key)
```

**How it works:**
- Files in vault paths are encrypted with `K_vault`
- Any vault member can decrypt `K_vault` using their private key
- With `K_vault`, they can decrypt any file in the vault
- Adding a member: encrypt `K_vault` with new member's public key, add to vault record
- Removing a member: rotate `K_vault`, re-encrypt for remaining members, re-encrypt affected data

**Vault paths:**
```
/vaults/engineering-team/          ← vault-encrypted with K_vault
/vaults/engineering-team/docs/     ← inherits vault encryption
/personal/alice/                   ← encrypted with alice's key only
```

Encryption inherits downward through the path hierarchy, just like permissions.

### Zero-Knowledge Storage Nodes

In a distributed (Raft) cluster:
- Storage nodes hold encrypted ciphertext
- They can replicate, serve, and manage chunks
- They CANNOT read the data
- Only users with the right vault key (or personal key) can decrypt
- The KV store, NVT, entry headers, and directory structure remain unencrypted (needed for operation)
- File CONTENT is encrypted; file METADATA (path, size, timestamps) may or may not be (configurable)

### Integration with Existing Systems

| System | Interaction with Encryption |
|---|---|
| **Auth (identity files)** | Identity contains the user's key pair. Vault keys encrypted per-user. |
| **Permissions (crudlify)** | Permissions checked BEFORE decryption. No key = no decrypt, but permissions add another layer. |
| **Parsers/Indexers** | Run on plaintext BEFORE encryption. Indexes are unencrypted (needed for queries). |
| **Versioning (forks/snapshots)** | Versions reference encrypted chunks. Same dedup applies. |
| **Replication (Raft)** | Nodes replicate ciphertext. Encryption is transparent to replication. |

### Open Design Questions

- [ ] Index encryption: should indexes themselves be encrypted? If so, queries require decryption of index data on every lookup — major performance hit. If not, index data leaks information (field values are visible even if file content is encrypted).
- [ ] Metadata encryption: should file paths, sizes, timestamps be encrypted? Full metadata encryption = maximum privacy but makes directory listings impossible without the key.
- [ ] Key rotation: when a vault member is removed, all data must be re-encrypted with a new vault key. For large vaults, this is expensive. Lazy re-encryption (re-encrypt on next read/write) vs eager (re-encrypt everything immediately)?
- [ ] Key derivation: derive encryption keys from passwords (PBKDF2/Argon2) or require explicit key files?
- [ ] Hardware security module (HSM) support for key storage?

### Cron Task Dependencies

Encryption adds work to the cron/background task system:
- **Key rotation jobs** — re-encrypt data when vault membership changes
- **Integrity verification** — decrypt + hash + compare for encrypted chunks
- **Index rebuilding** — requires decryption of all affected data
- **Garbage collection** — must consider vault-encrypted chunks (can't inspect content, only hashes)

### Why Deferred

This is the biggest single feature in the roadmap. It touches every layer: storage, indexing, auth, permissions, replication, cron. The architecture supports it (hash-first design, identity files, vault concept), but implementation requires careful design of the key management lifecycle, vault membership protocol, and the serial processing pipeline constraints.

---

## Multi-Database Sharding

**What:** Split a single logical database across multiple .aeordb files.

**Ideas:**
- Shard by hash prefix (chunk hashes are uniformly distributed)
- Each shard is a complete .aeordb file with its own NVT+KVS
- A coordinator layer routes operations to the correct shard

**Why deferred:** Single file handles terabytes. Sharding is for petabyte scale.

---

## Custom Rust MIME Detection Crate

**What:** Build our own MIME detection crate from the JavaScript `file-type` library (https://github.com/sindresorhus/file-type).

**Why discussed:** Wanted the best possible MIME detection from content bytes.

**Resolution:** Using `file-format` crate (200+ formats, zero deps) for now. Custom crate only if `file-format` proves insufficient.

---

## Query Engine Enhancements

Gaps identified during indexing design. Not blocking current implementation.

### Aggregations (SUM, AVG, MIN, MAX)
We have `.count()` but no other aggregations. Every analytics use case needs these. The NVT + sorted entries could accelerate MIN/MAX (first/last entry in sorted order).

### Sorting / ORDER BY
Results are unordered. Order-preserving converters mean index entries ARE roughly sorted by scalar — sorted iteration could leverage this instead of post-sort.

### Pagination / Cursors
`.limit()` and `.first()` exist but no `.offset()` or cursor-based paging. For cursor-based: use the last result's scalar as the starting point for the next page.

### Projections
Return only specific fields, not full FileRecords. Reduces bandwidth for large result sets. Requires the parser to extract fields on read (currently only on write).

### NULL / Missing Field Queries
`WHERE age IS NULL` needs to find documents NOT in the age index. Requires a "complement" approach: all documents at the path MINUS documents in the index.

### Query EXPLAIN
Show the user how a query will execute: which indexes, estimated selectivity, scan strategy. Like SQL's EXPLAIN. Essential for optimization.

### Index Selectivity / Statistics
The query engine should scan the most selective index first. Needs cardinality stats per index (distinct value count, entry count, min/max). Could be maintained incrementally on insert/delete.

### Type Coercion
`age > "30"` (string instead of number). Options: error (strict), coerce (lenient), configurable. Leaning toward error with a clear message.

### DISTINCT
Deduplicate results by a field value. Requires post-processing on the result set, or a distinct-values index.

### Compound Indexes
Index on `(age, city)` together. More selective than separate single-field indexes for multi-field queries. Could be implemented as a multi-dimensional scalar (concatenate scalars from multiple converters).

### Index Warm-up on Startup
Pre-load frequently used index NVTs on startup instead of lazy-loading on first query. Could be driven by access statistics from the previous session.

### Index Cache Invalidation on Version Restore
When restoring a snapshot, in-memory NVT caches may be stale. Need to invalidate or reload affected indexes.

### Trigram Indexing for Substring Search
Map trigrams to scalars. Each string produces multiple index entries (one per trigram). `CONTAINS 'smith'` = AND all trigram masks. Position-weighted scalars give prefix searches tighter buckets.

### GPU-Offloaded NVT Compositing
NVT masks are packed u64 bitsets — directly compatible with GPU compute shaders. Upload masks, run AND/OR/NOT kernels, download result. Database queries at framerate speeds.

### Metrics-Driven KV Growth Prediction
Use the metrics/event system to track write rate. If growth rate exceeds a threshold, skip one or two stages in the KV block stage table to proactively allocate headroom. Avoids frequent resizes during bulk imports.

### MMAP for KV Block Access
Memory-map the KV block instead of explicit seek+read. Let the OS page cache handle hot/cold page management. Simplifies the hot cache implementation — the OS IS the cache. Requires careful handling of resize (remap after growth).

### Concurrent KV Readers During Write
Current model: single writer lock for all KV operations. At high concurrency, reads block behind writes. Allow concurrent readers via RwLock or lock-free read path (readers see a consistent snapshot while writer modifies a copy).

---

## URGENT: Event System (WebSocket + In-Process Channels)

**What:** Configurable event system for reacting to database mutations in real time. Two delivery mechanisms: in-process channels for Rust library consumers, WebSocket for HTTP/browser clients.

**Priority:** Urgent

### Architecture

The append-only engine already produces events implicitly — every write IS a mutation. The event system makes these explicit and subscribable.

**In-process (Rust library):**
- `tokio::broadcast` channel on `StorageEngine` (or a wrapping `EventBus` struct)
- `engine.subscribe() -> broadcast::Receiver<EngineEvent>`
- Zero network overhead — direct channel send on every mutation
- Backpressure via bounded channel (lagging receivers skip events)

**Remote (HTTP clients):**
- WebSocket endpoint at `/ws/events`
- Server bridges the internal broadcast channel to WebSocket frames
- Clients send subscribe/unsubscribe messages with filters (path prefix, event types)
- JSON frames: `{"event": "file_stored", "path": "/people/smith.json", "timestamp": 1234567890}`
- Alternative: SSE at `/events/stream` for simpler clients (one-directional, no subscription management)
- WebSocket preferred — bidirectional enables dynamic subscription filtering

**Event types:**
- `FileStored` — path, content_type, size, hash
- `FileDeleted` — path
- `FileUpdated` — path, content_type, size, old_hash, new_hash
- `IndexUpdated` — path, field_name, strategy, entry_count
- `SnapshotCreated` — name, root_hash
- `SnapshotRestored` — name
- `ForkCreated` — name
- `ForkPromoted` — name
- `UserCreated` — user_id, username
- `UserDeactivated` — user_id
- `GroupChanged` — group_name
- `PermissionChanged` — path
- `ParserInvoked` — path, parser_name, success/failure
- `Error` — path, error_type, message

**Subscription filtering (WebSocket):**
```json
// Client sends:
{"action": "subscribe", "filter": {"path_prefix": "/people/", "events": ["file_stored", "file_deleted"]}}
{"action": "unsubscribe", "filter_id": "abc123"}

// Server sends:
{"event": "file_stored", "path": "/people/smith.json", "size": 1234, "timestamp": 1234567890}
```

**Optional per-directory event hooks:** `.config/events.json` could define WASM plugins invoked on specific events (like database triggers). Deferred to a later phase — start with passive observation, add active hooks later.

**Why urgent:** Events are the foundation for:
- Real-time dashboard updates (portal can subscribe instead of polling)
- Backup triggers (diff export on every N events)
- Replication (followers subscribe to leader's event stream)
- Webhooks (future — bridge events to HTTP callbacks)
- Client-side cache invalidation
- Audit logging

---

## URGENT: Backup System (Diff-Based .aeordb Export)

**What:** Incremental backup system using the append-only storage model. Full snapshots + lightweight diffs. Backup output is a valid .aeordb file.

**Priority:** Urgent

### Core Insight

The storage engine is append-only. Every entry has a file offset. A snapshot records the state at a point in time. Therefore, a diff between two snapshots is literally the byte range of entries written between those two offsets. This is trivially extractable with zero computation.

### Backup Chain Model

```
Full(A) → Diff(A→B) → Diff(B→C) → Diff(C→D) → Full(D) → Diff(D→E)
```

- **Full backup:** Complete .aeordb file (copy of the data file, or a new .aeordb with all live entries rebuilt — excluding voids/deleted)
- **Diff backup:** A new .aeordb file containing ONLY the entries written since the last backup point

### Why .aeordb as the backup format

- Backups are openable with `StorageEngine::open` — you can query them, inspect them, verify them
- Restore = merge diff .aeordb entries into target database
- No custom format to maintain — the backup format IS the database format
- Backup verification = open it and count entries. If it opens, it's valid.
- Chain restore: open base.aeordb, then replay diff1.aeordb, diff2.aeordb, etc.

### Diff Mechanics

```
Diff export (A → B):
  1. Record A's entry offset (from snapshot metadata or HEAD at backup time)
  2. Record B's current offset
  3. Create a new .aeordb file
  4. Copy all entries from offset_A to offset_B into the new file
  5. Build KV block + NVT for the diff file
  6. Write file header with metadata (parent_snapshot, created_at, etc.)
```

The diff .aeordb is a fully self-contained database — you can open it, query it, see what changed.

### CLI Interface

```bash
# Full backup
aeordb backup full --database data.aeordb --output backup-2026-04-04.aeordb

# Diff backup (since last full or last diff)
aeordb backup diff --database data.aeordb --output diff-2026-04-04-001.aeordb

# Restore from chain
aeordb restore --base backup-2026-04-01.aeordb --diffs diff-001.aeordb,diff-002.aeordb --output restored.aeordb

# Verify a backup
aeordb backup verify --file backup-2026-04-04.aeordb
```

### HTTP API

```
POST /admin/backup/full → streams the .aeordb file
POST /admin/backup/diff?since=snap1 → streams the diff .aeordb
POST /admin/restore → accepts .aeordb file, merges into current database
GET /admin/backup/status → list backup history (snapshots used as backup points)
```

### Text-Based Export (Secondary)

For grep-friendly exports, JSON Lines format:
```
{"type":"file","path":"/people/smith.json","content_type":"application/json","data":"eyJuYW1lIjoiSm9obiJ9","hash":"a1b2c3..."}
{"type":"file","path":"/people/jones.json","content_type":"application/json","data":"eyJuYW1lIjoiSmFuZSJ9","hash":"d4e5f6..."}
{"type":"directory","path":"/people/","children":["smith.json","jones.json"]}
```

Binary content is base64-encoded (unavoidable for text format). This format is secondary to .aeordb — useful for debugging, auditing, and piping through text tools, but not the primary backup mechanism.

### Automatic Backup Schedule (Future)

Ties into the cron/background task system (also a future plan):
- Configure backup schedule: full every Sunday, diff every hour
- Retain N full backups + diffs between them
- Auto-prune old backup chains

**Why urgent:** A database without backups is a liability. Users need to trust that their data is recoverable. The append-only architecture makes this almost free to implement — we're not doing complex journaling or WAL replay. It's just copying byte ranges.

---

## HIGH PRIORITY: Built-In Parser Plugins

**What:** Ship a library of WASM parser plugins with AeorDB. Each parser is a separate crate compiled to `wasm32-unknown-unknown`, deployed to the database on first boot or via CLI.

**Priority:** High (depends on document-parsers feature)

### Parser Library

| Parser | Input | Output JSON | Crate Dependencies |
|--------|-------|------------|-------------------|
| `plaintext-parser` | Plain text files | `{"text": "...", "line_count": N, "word_count": N, "byte_count": N}` | None |
| `csv-parser` | CSV files | `{"headers": [...], "rows": [...], "row_count": N, "column_count": N}` | `csv` crate (pure Rust) |
| `xml-parser` | XML files | `{"root": {...}, "namespaces": [...]}` (JSON mirror of XML tree) | `quick-xml` (pure Rust) |
| `markdown-parser` | Markdown files | `{"text": "...", "headings": [...], "links": [...], "code_blocks": [...]}` | `pulldown-cmark` (pure Rust) |
| `image-metadata` | JPEG/PNG/etc. | `{"metadata": {"width": N, "height": N, "format": "...", "exif": {...}}}` | `kamadak-exif` or `rexif` |
| `audio-metadata` | MP3/FLAC/etc. | `{"metadata": {"title": "...", "artist": "...", "album": "...", "duration_ms": N}}` | `id3` or `lofty` |
| `pdf-metadata` | PDF files | `{"metadata": {"title": "...", "author": "...", "page_count": N}, "text": "..."}` | `lopdf` or `pdf-extract` |
| `source-code` | .rs/.js/.py/etc. | `{"language": "...", "imports": [...], "functions": [...], "line_count": N}` | `tree-sitter` (ambitious) or regex-based |

### Architecture

```
aeordb-parsers/
  plaintext/
    Cargo.toml
    src/lib.rs
  csv/
    Cargo.toml
    src/lib.rs
  xml/
    Cargo.toml
    src/lib.rs
  ...
```

Each parser crate:
- Targets `wasm32-unknown-unknown`
- Exports `handle(ptr, len) -> i64` per the plugin protocol
- Receives the parser envelope (base64 data + metadata)
- Returns JSON
- Has its own test suite (native tests, not WASM)
- Built independently, versioned independently

### Deployment

- **Auto-deploy on first boot:** `aeordb-cli start` checks if built-in parsers are deployed at `/.parsers/`. If not, deploys them.
- **CLI deploy:** `aeordb-cli parsers install` deploys all built-in parsers. `aeordb-cli parsers install plaintext` deploys one.
- **Global registry auto-populated:** On deployment, updates `/.config/parsers.json` with content-type → parser mappings.

### Why high priority

- Dogfooding: proves the parser plugin system works end-to-end with real formats
- User value: out-of-the-box support for common file types without custom plugins
- Community model: parsers maintained separately, upgraded independently, contributed by community
- Test coverage: each parser exercises the full pipeline (WASM invocation, envelope, source resolution, indexing)

