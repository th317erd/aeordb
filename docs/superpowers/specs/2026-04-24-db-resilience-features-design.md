# Database Resilience Features — Design Spec

**Date:** 2026-04-24
**Status:** Approved

## Overview

Four features to further harden AeorDB beyond the core corruption hardening already implemented (scanner resilience, KV page recovery, hash verification, lost+found quarantine).

---

## 1. Auto-Snapshot Before GC

**Current:** `run_gc` does mark-and-sweep directly. If it sweeps something it shouldn't, the data is gone.

**Fix:** Before `gc_sweep` executes, create a snapshot named `_aeordb_pre_gc_{timestamp}`. Only create if there are entries to sweep (skip on no-op GC). Keep the last 3 pre-GC snapshots — delete older ones.

**Implementation:** In `gc.rs`, before calling `gc_sweep`:
1. Check if sweep would actually remove entries (mark phase returned > 0 unreachable)
2. If yes, create snapshot via `VersionManager::create_snapshot`
3. Clean up old pre-GC snapshots (list snapshots, filter by `_aeordb_pre_gc_` prefix, delete all but most recent 3)
4. Proceed with sweep

---

## 2. `aeordb verify` CLI Command

A new subcommand that performs a full integrity check and optionally repairs.

**Usage:**
```
aeordb verify -D /path/to/data.aeordb [--repair]
```

**What it checks:**
1. Scan every entry in the append log — verify magic, header, hash
2. Count entries by type (chunks, file records, directories, symlinks, snapshots, deletions, voids)
3. Calculate storage metrics (logical data, chunk data, dedup savings, void space, overhead)
4. For every FileRecord — verify all referenced chunks exist in KV
5. For every DirectoryIndex — verify all listed children exist in KV
6. Compare KV index against append log — find stale or missing entries
7. Find orphaned files (exist by path hash but not listed in parent directory)

**Output:** Full health report with entry summary, integrity results, storage metrics, directory consistency, KV index status.

**`--repair` flag auto-fixes:**
1. KV index → rebuild from append log (existing `rebuild_kv()`)
2. Unlisted files → re-run `propagate_to_parents`
3. Missing children → remove from directory listing, quarantine reference to `lost+found/`
4. Corrupt entries → quarantine to `lost+found/`
5. Orphaned chunks → report only (GC handles cleanup)

**Repair never deletes data.** It rebuilds indexes, re-links references, or quarantines to `lost+found/`.

**Implementation:**
- New file `aeordb-lib/src/engine/verify.rs` — the verification logic, returns a structured `VerifyReport`
- New subcommand in `aeordb-cli/src/commands/verify.rs` — CLI wrapper that opens the DB, runs verify, prints the report
- Register in `aeordb-cli/src/main.rs`

---

## 3. Background Integrity Scanner

A background tokio task that periodically spot-checks random entries for silent corruption (bit rot).

**Behavior:**
- Spawns on engine startup (like heartbeat/metrics pulse)
- Every N minutes (configurable, default 60), picks a random sample of entries from KV
- Reads each entry and verifies its hash
- If corruption found → quarantine to `lost+found/`, log warning, attempt cluster healing if peers available
- Reports stats: `integrity_checks_total`, `integrity_failures_total`

**Sample size:** ~1% of entries per cycle, minimum 10, maximum 1000. Full coverage in ~4 days for a typical database.

**Not a full scan** — that's what `aeordb verify` is for. This is a canary for early detection.

**Implementation:**
- New file `aeordb-lib/src/engine/integrity_scanner.rs`
- `spawn_integrity_scanner(engine: Arc<StorageEngine>, interval_minutes: u64)` → returns `JoinHandle`
- Launched in the server startup alongside heartbeat and metrics pulse

---

## 4. Cluster Auto-Healing

When corruption is detected and the node has peers, attempt to recover the corrupt entry from a peer before quarantining.

**Hooks into:**
- `get_entry` returning `CorruptEntry` → try healing before propagating error
- Background scanner finding bad hash → try healing
- `aeordb verify --repair` finding corrupt chunk → try healing

**Flow:**
1. Extract the hash that failed verification
2. Check if `PeerManager` has configured peers
3. Request the chunk from peer via `POST /sync/chunks`
4. If peer returns valid data → re-verify hash, write locally, log info
5. If no peer has it → fall through to quarantine

**Implementation:**
- New file `aeordb-lib/src/engine/auto_heal.rs`
- `pub fn try_heal_from_peers(engine: &StorageEngine, peer_manager: &PeerManager, hash: &[u8]) -> bool`
- Returns true if healed, false if not
- Healed data MUST be re-verified after receiving (don't trust network)

---

## Testing Strategy

**Auto-snapshot before GC:**
- GC with deletions → `_aeordb_pre_gc_*` snapshot exists
- GC no-op → no snapshot created
- GC 5 times → only last 3 pre-GC snapshots kept

**`aeordb verify`:**
- Clean DB → all OK
- Corrupt hash → reported
- Orphaned file → reported as "unlisted"
- `--repair` → quarantines + rebuilds, re-verify clean

**Background scanner:**
- Inject corruption → detected within one cycle
- Clean DB → zero failures
- Respects sample size limits

**Cluster auto-healing:**
- Corrupt entry + healthy peer → healed, returns true
- Corrupt entry + no peers → quarantined, returns false
- Healed data re-verified after network receive

## Out of Scope

- Atomic multi-entry operations (deferred — hot file extension approach planned separately)
- Full database compaction (rewrite log omitting voids/corrupt entries)
- Automatic lost+found cleanup/expiry
