# Garbage Collection: Mark-and-Sweep — Spec

**Date:** 2026-04-08
**Status:** Approved
**Priority:** Medium — databases grow forever without it

---

## 1. Overview

Manual mark-and-sweep GC. The user decides when to run it. No automatic triggers, no ref counting, no deletion cascades.

**Mark:** Walk all live version trees (HEAD + snapshots + forks). Collect every reachable hash.

**Sweep:** Everything in the KV store not in the reachable set is garbage. Mark as void (reclaimable space).

---

## 2. What's "live"?

A hash is live if it's reachable from ANY of:
- HEAD (current state)
- Any snapshot's root_hash
- Any fork's root_hash

Reachable means: starting from a root hash, recursively follow all B-tree nodes, DirectoryIndex entries, ChildEntry hashes, FileRecord hashes, and chunk hashes. If you can walk from a root to an entry, it's live.

Everything else — old B-tree nodes from structural sharing, orphaned chunks from deleted files, stale FileRecords — is garbage.

---

## 3. Algorithm

```
fn gc(engine: &StorageEngine) -> GcResult {
    // MARK: collect all reachable hashes
    let mut live: HashSet<Vec<u8>> = HashSet::new();

    // Walk HEAD
    let head = engine.head_hash()?;
    walk_and_mark(engine, &head, &mut live)?;

    // Walk every snapshot
    let vm = VersionManager::new(engine);
    for snapshot in vm.list_snapshots()? {
        walk_and_mark(engine, &snapshot.root_hash, &mut live)?;
        // Also mark the snapshot entry itself
        live.insert(snapshot_key_hash);
    }

    // Walk every fork
    for fork in vm.list_forks()? {
        walk_and_mark(engine, &fork.root_hash, &mut live)?;
        live.insert(fork_key_hash);
    }

    // SWEEP: iterate all KV entries, mark non-live as void
    let all_entries = engine.kv_store.iter_all()?;
    let mut garbage_count = 0;
    let mut reclaimed_bytes = 0;

    for entry in &all_entries {
        if !live.contains(&entry.hash) && !entry.is_deleted() {
            // This entry is garbage
            engine.mark_entry_deleted(&entry.hash)?;
            garbage_count += 1;
            // Optionally: register as void for space reuse
        }
    }

    GcResult {
        versions_scanned: snapshot_count + fork_count + 1,
        live_entries: live.len(),
        garbage_entries: garbage_count,
        reclaimed_bytes,
    }
}
```

### walk_and_mark

Recursively walks a version tree and marks every hash as live:

```
fn walk_and_mark(engine, root_hash, live) {
    if live.contains(root_hash) { return; } // already visited (structural sharing)
    live.insert(root_hash);

    let entry = engine.get_entry(root_hash)?;
    match entry.type {
        DirectoryIndex => {
            // Could be flat list or B-tree node
            if is_btree_format(data) {
                // Mark this B-tree node
                // Parse node, recurse into children (internal nodes) or mark entries (leaf nodes)
                for child_hash in node.children { walk_and_mark(engine, child_hash, live); }
                for entry in leaf.entries {
                    live.insert(entry.hash);
                    // If entry is a file, walk its FileRecord for chunks
                    walk_file_record(engine, &entry.hash, live);
                }
            } else {
                // Flat directory: mark all child hashes
                for child in children {
                    live.insert(child.hash);
                    walk_and_mark(engine, &child.hash, live); // recurse for directories
                    walk_file_record(engine, &child.hash, live); // chunks for files
                }
            }
        }
        FileRecord => {
            // Mark all chunk hashes
            for chunk_hash in file_record.chunk_hashes {
                live.insert(chunk_hash);
            }
        }
        Chunk => { /* leaf — no children */ }
        _ => { /* snapshot, fork, deletion records, voids — skip children */ }
    }
}
```

The key optimization: `if live.contains(root_hash) { return; }` — structural sharing means the same B-tree node appears in multiple versions. We only walk it once.

---

## 4. CLI

```bash
aeordb gc --database data.aeordb

# Output:
# AeorDB Garbage Collection
# Versions scanned: 5 (1 HEAD + 3 snapshots + 1 fork)
# Live entries: 150,000
# Garbage entries: 23,000
# Reclaimed: 45 MB
# Duration: 1.2s
```

Add `--dry-run` flag that reports what WOULD be collected without actually deleting:

```bash
aeordb gc --database data.aeordb --dry-run

# Output:
# [DRY RUN] Would collect 23,000 garbage entries (45 MB)
```

---

## 5. HTTP API

```
POST /admin/gc
POST /admin/gc?dry_run=true
```

Response:
```json
{
    "versions_scanned": 5,
    "live_entries": 150000,
    "garbage_entries": 23000,
    "reclaimed_bytes": 47185920,
    "duration_ms": 1200,
    "dry_run": false
}
```

Requires admin auth (root user).

---

## 6. What gets collected

- Old B-tree nodes from structural sharing (no longer referenced by any version)
- Orphaned chunks from deleted/overwritten files
- Stale FileRecords from deleted files (already marked deleted, but still on disk)
- Old content-addressed directory entries superseded by newer versions
- DeletionRecords themselves (they served their purpose)
- Orphaned index entries (.indexes/ files for deleted paths)

---

## 7. What does NOT get collected

- Anything reachable from HEAD, any snapshot, or any fork
- The file header
- Void entries (they're already reclaimable space markers)
- System tables (users, groups, API keys) — these are always live
- The KV file (.aeordb.kv) — separate file, not subject to GC

---

## 8. System table protection

System tables (users, groups, API keys, config) are stored via `SystemTables` which uses the engine. Their entries must be marked as live during the mark phase.

Add system table roots to the mark phase:
```
// Mark system table entries as live
for entry in engine.entries_by_type(KV_TYPE_FILE_RECORD)? {
    if path.starts_with("/.system/") || path.starts_with("/.config/") {
        live.insert(entry.hash);
        // Also mark their chunks
    }
}
```

Or simpler: any path starting with `/.system/` or `/.config/` is always live.

---

## 9. GC events

Emit an event when GC runs:

```json
{
    "event_type": "gc_completed",
    "payload": {
        "versions_scanned": 5,
        "live_entries": 150000,
        "garbage_entries": 23000,
        "reclaimed_bytes": 47185920,
        "duration_ms": 1200
    }
}
```

---

## 10. Edge cases

- **GC during writes:** The engine is single-writer. GC acquires the write lock. No concurrent writes during GC. This is fine — GC is a maintenance operation.
- **Empty database:** 0 garbage. No-op.
- **No snapshots/forks:** Only HEAD is live. Everything not reachable from HEAD is garbage.
- **All versions share everything:** Structural sharing dedup means walk_and_mark is fast (visited set short-circuits).
- **Very large databases:** Mark phase is O(total live entries). Sweep is O(total KV entries). Both are linear. At 1M entries, expect seconds, not minutes.

---

## 11. Implementation phases

### Phase 1 — Mark phase (walk_and_mark)
- Recursive tree walker that collects all reachable hashes
- Handles B-tree nodes, flat directories, FileRecords, chunks
- Structural sharing optimization (skip already-visited)
- Tests: mark on simple tree, mark with snapshots, mark with B-tree

### Phase 2 — Sweep phase
- Iterate KV entries, mark non-live as deleted
- Count garbage entries and estimate reclaimed bytes
- GcResult struct
- Tests: sweep finds garbage, sweep preserves live, dry_run mode

### Phase 3 — CLI command
- `aeordb gc --database <path> [--dry-run]`
- Output formatting

### Phase 4 — HTTP endpoint
- `POST /admin/gc [?dry_run=true]`
- Admin auth required
- GC event emission

---

## 12. Non-goals (deferred)

- Automatic GC scheduling (needs cron system)
- Void consolidation (merging adjacent voids — separate feature)
- File defragmentation (rewriting file to eliminate voids — separate feature)
- Incremental GC (partial collection without full mark phase)
- Concurrent GC (running GC while writes continue)
