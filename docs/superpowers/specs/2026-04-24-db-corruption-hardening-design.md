# Database Corruption Hardening — Design Spec

**Date:** 2026-04-24
**Status:** Approved
**Severity:** Critical — addresses data loss and permanent write failures

## Problem

When corruption exists in the append log (`.aeordb` file), the database can:
1. **Stop scanning early** — all entries after corruption are lost from the KV index
2. **Permanently break all writes** — a single corrupt KV bucket page fails every flush
3. **Refuse to open** — IO errors during scan abort the entire rebuild
4. **Silently return garbage** — direct reads don't verify hashes
5. **Never recover at runtime** — no self-healing path without restart

The root principle: **user data is sacred. Never drop anything. Quarantine corrupt data to `lost+found/` so users can attempt manual recovery.**

## Architecture

Five interdependent hardening layers:

### 1. Scanner Resilience (P0 + P2)

**Current:** Scanner hits corrupt header → `return None` → iteration stops → all entries after corruption lost. IO errors → `?` propagates → database refuses to open.

**Fix — Magic byte search:**
When `EntryHeader::deserialize` fails, instead of returning `None`:
1. Scan forward byte-by-byte from `current_offset + 1` looking for the entry magic bytes (`0x0AE012DB`)
2. Cap the search window at 1MB to avoid scanning the entire file
3. When found, validate the candidate header (deserialize + sanity checks on lengths)
4. If valid, resume scanning from there
5. Quarantine the skipped region (raw bytes) to `{parent}/lost+found/scan_{offset}_{timestamp}.bin`

**Fix — IO error tolerance:**
In `storage_engine.rs` `open_internal`, change `let scanned = scanned_result?;` to:
```rust
let scanned = match scanned_result {
    Ok(entry) => entry,
    Err(e) => {
        tracing::warn!("Skipping corrupt entry during rebuild: {}", e);
        continue;
    }
};
```

### 2. KV Store Resilience (P1 + P4)

**Current:** Corrupt KV page → `deserialize_page` returns `Err` → `flush()` uses `?` → entire flush fails → all writes permanently broken.

**Fix — Corrupt page recovery during flush:**
When `deserialize_page` fails for a specific bucket:
1. Quarantine the raw page bytes to `lost+found/kvpage_{bucket}_{timestamp}.bin`
2. Zero out the page (reset to empty)
3. Log a warning with the bucket index
4. Continue flushing other buckets

**Fix — Runtime KV rebuild:**
Add `rebuild_kv(&self) -> EngineResult<()>` to `StorageEngine`:
1. Acquire write locks on both writer and kv_writer
2. Delete the `.kv` file
3. Rescan the append log using the hardened scanner
4. Create a fresh KV from the scan
5. Publish a new snapshot

Triggered two ways:
- **Auto-trigger:** When flush resets a corrupt page, schedule rebuild (background or inline)
- **Manual:** `POST /system/repair` admin endpoint

### 3. Hash Verification on Direct Reads (P3)

**Current:** `read_entry_at_shared` reads header, key, value — no hash verification. Bit-flipped values silently returned as valid.

**Fix:** After reading key and value in `read_entry_at_shared`, call `header.verify(&key, &value)`. If verification fails, return `EngineError::CorruptEntry` with the offset.

Performance cost: one BLAKE3 hash per read (~microseconds for typical entries). Worth it for data integrity.

### 4. Lost+Found Quarantine System

**Principle:** Never drop data. Quarantine to `lost+found/` as a sibling of the problem entry.

**Location rules:**

| Scenario | Lost+found location |
|----------|-------------------|
| `/docs/readme.md` has corrupt chunk | `/docs/lost+found/readme.md_{timestamp}.bin` |
| `/images/photo.jpg` hash mismatch | `/images/lost+found/photo.jpg_{timestamp}.json` |
| Directory `/data/` index corrupt | `/data/lost+found/dir_index_{timestamp}.bin` |
| Scanner can't parse entry (unknown parent) | `/lost+found/scan_{offset}_{timestamp}.bin` |
| KV page corruption (index-level) | `/lost+found/kvpage_{bucket}_{timestamp}.bin` |

**Implementation:** `lost_found.rs` module:
```rust
pub fn quarantine_bytes(engine: &StorageEngine, parent_path: &str, filename: &str, reason: &str, data: &[u8])
pub fn quarantine_metadata(engine: &StorageEngine, parent_path: &str, filename: &str, reason: &str, metadata: &serde_json::Value)
```

Writes to `{parent_path}/lost+found/{filename}` using `DirectoryOps::store_file` with `RequestContext::system()`.

**`lost+found/` is a regular visible directory** — no dot prefix. Users see it immediately in the file browser if something went wrong.

**Failure tolerance:** If quarantining fails (disk full, engine error), log a warning and continue. The parent operation must never fail because of a quarantine write failure.

### 5. Cluster Auto-Healing

When corruption is detected and the node is in replication mode:

1. Extract the chunk hash or entry hash that failed verification
2. Check if peers are configured (via `PeerManager`)
3. Request the chunk from peers via existing `POST /sync/chunks` endpoint
4. If a peer returns valid data (hash-verified):
   - Write it locally — entry is healed
   - Log info: "Auto-healed corrupt entry at {path} from peer {node_id}"
   - Skip quarantine (data recovered)
5. If no peer has it (or no peers configured):
   - Quarantine to `lost+found/` as described above

This means in a healthy cluster, corruption self-heals transparently. `lost+found/` only accumulates entries that are truly unrecoverable.

### 6. Graceful Error Propagation (P5 + P6)

**`entries_by_type`:** Change `?` to match-and-continue. Return successfully-read entries, skip corrupt ones.

**`read_file` with corrupt chunk:** Return `EngineError::CorruptEntry` (not generic IO error). HTTP handler maps to 500 with message: "File exists but has corrupt data. Check /lost+found/ for quarantined content."

**`list_directory` with corrupt child:** Skip the corrupt child entry, log warning, return the other children. Don't fail the entire listing because one child is damaged.

## Testing Strategy

**Scanner resilience:**
- Inject corrupt bytes mid-file → scanner skips past, finds valid entries after, quarantines to `lost+found/`
- Inject at multiple offsets → scanner recovers from each
- Truncated file at EOF → scanner stops gracefully
- IO error during rebuild → logs warning, continues, database opens

**KV page resilience:**
- Corrupt a single KV bucket page → flush resets it, writes succeed, page bytes quarantined
- Corrupt multiple pages → all reset, writes continue
- Auto KV rebuild after page reset → all entries re-indexed

**Hash verification:**
- Bit-flip a stored value → `get_entry` returns `CorruptEntry` error
- Valid entry → verification passes, data returned correctly

**Lost+found:**
- Quarantined data appears at correct sibling `lost+found/` path
- Quarantine metadata is valid JSON with offset, reason, timestamp
- Failed quarantine writes don't crash parent operation
- `lost+found/` is a regular visible directory

**Runtime rebuild:**
- Corrupt KV page → writes fail → auto-rebuild → writes succeed → all entries recovered

**Cluster auto-healing:**
- Corrupt entry on node A with healthy peer B → node A requests chunk from B → healed
- Corrupt entry with no peers → quarantined to `lost+found/`

## Out of Scope

- Full database compaction (rewrite log omitting corrupt entries) — future optimization
- Automatic `lost+found/` cleanup or expiry — manual user decision
- Partial file recovery (returning non-corrupt chunks of a multi-chunk file) — complex, low value
