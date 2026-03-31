# Future Plans

Ideas discussed but deferred. Not blocking current implementation.

---

## Auth Provider URI System (Implement Soon)

**What:** The `--auth` CLI flag accepts a URI that determines how authentication is resolved. One flag, multiple modes, extensible via URI scheme.

**Modes:**

| Value | Meaning |
|---|---|
| `--auth=false` (or `null`, `no`, `0`) | No auth. Dev mode, zero ceremony. All requests allowed. |
| `--auth=self` | Per-database auth. Keys stored in the `.aeordb` file itself. **(Current default)** |
| `--auth=./` | Same as `self` (explicit "look here"). |
| `--auth=file://~/.config/aeordb/identity` | Shared local identity file. SSH-like pattern. |
| `--auth=file:///etc/aeordb/cluster-identity` | System-wide identity file. |
| `--auth=https://auth.mycompany.com/aeordb` | Remote auth service (future). |
| `--auth=ssh://admin@auth-server/identity` | SSH-tunneled identity (future). |

**Architecture:**

```rust
trait AuthProvider: Send + Sync {
  fn validate_api_key(&self, key: &str) -> Result<ApiKeyRecord>;
  fn signing_key(&self) -> Result<JwtManager>;
  fn store_api_key(&self, record: &ApiKeyRecord) -> Result<()>;
  fn list_api_keys(&self) -> Result<Vec<ApiKeyRecord>>;
  fn revoke_api_key(&self, key_id: &str) -> Result<()>;
  // magic links, refresh tokens, etc.
}
```

**Implementations to build now:**
- `FileAuthProvider` — opens a `.aeordb` file, uses SystemTables. Handles `file://` and `self`.
- `NoAuthProvider` — allows everything. Handles `false`/`null`/`no`/`0`.

**Implementations for later:**
- `HttpAuthProvider` — delegates to an HTTP auth service. Handles `https://`.
- `SshAuthProvider` — tunnels through SSH. Handles `ssh://`.

**Identity file format:** An `.aeordb` file containing only auth data (signing keys, API key hashes, user metadata). Same engine format — the identity file IS a tiny database.

**Use case:** A developer running multiple local databases shares one identity at `~/.config/aeordb/identity`. No per-database bootstrap ceremony. Create a new database, it just works with the same key.

**Default behavior:** `--auth=self` (current behavior). Backward compatible.

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
