# AeorDB Full Code Audit Report

**Date:** 2026-04-16
**Auditors:** 5 parallel review agents (security, data integrity, code quality, concurrency, edge cases)

---

## CRITICAL Findings (Fix immediately)

| # | Source | Issue | File(s) |
|---|--------|-------|---------|
| C1 | Data Integrity | **All remote sync write errors silently discarded** — `let _ =` on every store_entry, store_file, delete_file in do_sync_cycle_remote. Disk-full or I/O error = permanent silent data loss. Sync state saved as if everything succeeded. | sync_engine.rs:495+ |
| C2 | Edge Cases | **Silent u16 truncation on paths > 65535 bytes** — `path_bytes.len() as u16` wraps without checking. Corrupts serialization, produces garbage on deserialization. Affects ALL record types. | file_record.rs:45, directory_entry.rs:35, symlink_record.rs:30, deletion_record.rs:27, version_manager.rs:53 |

## HIGH Findings (Fix before production)

| # | Source | Issue | File(s) |
|---|--------|-------|---------|
| H1 | Security | **`/engine/_hash/{hex_hash}` bypasses /.system/ access control** — any authenticated user can read system entries (JWT signing key, API keys) by hash. No is_system_path or FLAG_SYSTEM check. | engine_routes.rs:720-780 |
| H2 | Security | **FLAG_SYSTEM is never set** — system_chunk_hash, system_file_identity_hash, store_entry_with_flags are all dead code. System entries stored with flags=0, indistinguishable from user data at storage layer. | entry_header.rs, directory_ops.rs, storage_engine.rs |
| H3 | Security | **Symlink to /.system/ bypasses access control** — non-root user creates symlink pointing to /.system/config/jwt_signing_key, reads through symlink. Target not validated against system paths. | symlink_routes.rs, engine_routes.rs |
| H4 | Security | **chunk_hashes_needed leaks system hashes** — sync diff response includes chunk hashes from filtered-out entries. Built before filtering applied. | sync_routes.rs:508-518 |
| H5 | Data Integrity | **Conflict store errors silently discarded** — `let _ = store_conflict(...)`. Lost conflicts = silent data corruption without user awareness. | sync_engine.rs:222 |
| H6 | Data Integrity | **No hash verification on point reads** — `read_entry_at_shared` never verifies the entry hash. Bit flips on disk served silently. | append_writer.rs:265 |
| H7 | Data Integrity | **Unbounded allocation from corrupt headers** — `vec![0u8; header.value_length as usize]` with no sanity check. Corrupt header with value_length=0xFFFFFFFF causes 4GB allocation → OOM crash. | append_writer.rs:251, entry_scanner.rs:68 |
| H8 | Data Integrity | **Hot file truncation not fsynced** — crash between truncate and next flush could replay stale entries including deleted ones. | disk_kv_store.rs:883 |
| H9 | Data Integrity | **insert() swallows flush and hot-buffer errors** — returns () so callers have no way to know writes are failing. Write buffer grows unboundedly. | disk_kv_store.rs:315-347 |
| H10 | Edge Cases | **i64 as u64 cast inverts LWW ordering for negative timestamps** — negative updated_at wraps to very large u64, making earliest write "win" instead of latest. | merge.rs:192-193 |
| H11 | Edge Cases | **Null bytes in paths create identity hash collisions** — null byte separator in hash input means path "/foo\0bar" with empty content_type == path "/foo" with content_type "bar". | directory_ops.rs:67-76 |
| H12 | Edge Cases | **Unsanitized symlink target in HTTP header** — header injection via \r\n in symlink target placed in X-Symlink-Target. Path not sanitized unlike other headers. | engine_routes.rs:226,661 |
| H13 | Edge Cases | **Sync chunks stored without hash integrity verification** — remote peer can send arbitrary data under any hash key. No recomputation/verification before storing. | sync_api.rs:184-195 |
| H14 | Edge Cases | **from_utf8_lossy masks corruption** — SymlinkRecord and SnapshotInfo use lossy UTF-8 decoding, silently replacing invalid bytes. FileRecord correctly uses from_utf8 with error. Inconsistent. | symlink_record.rs:67,80; version_manager.rs:96,199 |
| H15 | Edge Cases | **Random UUID fallback on malformed claims.sub** — `unwrap_or(Uuid::new_v4())` masks parse failures, generates random identity instead of explicit error. 7 occurrences. | engine_routes.rs:66,142,271,402,467,589,646 |
| H16 | Code Quality | **Triplicated hex_encode** — identical function copy-pasted 3 times when hex crate is already in dependencies. | api_key.rs:93, refresh.rs:37, magic_link.rs:36 |
| H17 | Code Quality | **Four incompatible require_root helpers** — same guard function implemented 4 times with inconsistent return types. | admin_routes.rs, cluster_routes.rs, task_routes.rs, conflict_routes.rs |
| H18 | Concurrency | **TaskQueue dequeue_next not atomic** — task claimed without atomically marking as Running. Double-dequeue possible with multiple workers. | task_queue.rs:137-153 |

## MEDIUM Findings (Fix soon)

| # | Source | Issue | File(s) |
|---|--------|-------|---------|
| M1 | Security | Cluster secret comparison not constant-time (timing side-channel) | sync_routes.rs:127 |
| M2 | Security | Magic link JWT uses email as sub — fails permission_middleware UUID check | routes.rs:644-653 |
| M3 | Security | No request body size limit on sync endpoint item counts | sync_routes.rs |
| M4 | Security | Rate limiter HashMap unbounded — DoS via unique emails | rate_limiter.rs:31 |
| M5 | Security | /upload/config public, leaks hash algorithm details | mod.rs:347 |
| M6 | Data Integrity | apply_merge_operations NOT atomic despite comment — no rollback | sync_apply.rs:8-54 |
| M7 | Data Integrity | No parent directory fsync after KV file rename | disk_kv_store.rs:536 |
| M8 | Data Integrity | Sync loop no graceful shutdown (abort drops mid-sync) | sync_engine.rs:667 |
| M9 | Data Integrity | 10 GB upload fully buffered in memory | mod.rs:237 |
| M10 | Data Integrity | GC loads all KV entries into memory twice | gc.rs:81 |
| M11 | Concurrency | TaskQueue enqueue registry update not atomic — concurrent enqueues lose entries | task_queue.rs:109-134 |
| M12 | Concurrency | Peer sync lock not released on panic — permanently blocks future syncs | sync_engine.rs:89-122 |
| M13 | Concurrency | PeerManager silently swallows poisoned lock errors | peer_connection.rs:66-158 |
| M14 | Concurrency | TaskQueue .unwrap() on RwLock — poison propagation | task_queue.rs:243+ |
| M15 | Edge Cases | Empty path normalizes to "/" — stores ghost root entry | path_utils.rs:4 |
| M16 | Edge Cases | Self-referencing symlink accepted at creation (detected at read) | directory_ops.rs:932-942 |
| M17 | Edge Cases | Unbounded recursive listing depth from client | engine_routes.rs:366 |
| M18 | Edge Cases | Eager chunk loading exhausts memory for large files | directory_ops.rs:163-194 |
| M19 | Code Quality | System-path filtering copy-pasted 3 times in engine_get | engine_routes.rs |
| M20 | Code Quality | engine_get is ~490 lines — should be broken up | engine_routes.rs:133-489 |
| M21 | Code Quality | 10 create_app_* functions — should be a builder | mod.rs |

## LOW Findings (Nice to fix)

| # | Source | Issue |
|---|--------|-------|
| L1 | Code Quality | Dead code with #[allow(dead_code)] — void_manager, query fns, batch_commit file_key |
| L2 | Code Quality | Error response inconsistency — ErrorResponse vs inline JSON (50+ inline occurrences) |
| L3 | Code Quality | DiskKVStore::create and create_at_stage share identical init logic |
| L4 | Code Quality | Peer status serialization duplicated in cluster_routes |
| L5 | Code Quality | sync_routes has 6 near-identical filter-then-push loops |
| L6 | Code Quality | try_initialize_metrics trivial wrapper (adds nothing) |
| L7 | Concurrency | Cache thundering herd on cold miss (harmless, idempotent) |
| L8 | Concurrency | NVT cloned on every flush even when unchanged |
| L9 | Concurrency | stats() acquires kv_writer Mutex just for file path |
| L10 | Edge Cases | Malformed conflict records silently dropped in list_conflicts_typed |
| L11 | Edge Cases | Non-existent base hash silently triggers full re-sync |
| L12 | Security | Error messages include internal paths/hashes |
| L13 | Security | Glob pattern injection — no complexity limit |
| L14 | Security | No expiry cleanup for magic links and refresh tokens |

---

## Top 10 Priority Fixes

1. **C1** — Propagate errors in sync_engine remote writes (prevents silent data loss)
2. **H1+H2+H3** — Wire up FLAG_SYSTEM, enforce on hash-based reads, validate symlink targets (security boundary)
3. **C2** — Add u16 length checks on serialization (prevents data corruption)
4. **H7** — Validate key/value lengths against total_length before allocation (prevents OOM crash)
5. **H6** — Add hash verification on point reads (detects disk corruption)
6. **H13** — Verify chunk hash integrity before storing sync data (prevents data poisoning)
7. **H11** — Reject null bytes in paths or use length-prefixed hash encoding (prevents hash collisions)
8. **H12** — Sanitize symlink targets in HTTP headers (prevents header injection)
9. **H15** — Replace UUID::new_v4() fallback with explicit error (prevents identity confusion)
10. **M1** — Use constant-time comparison for cluster secret (prevents timing attack)

---

## Positive Observations

The auditors unanimously praised:
- **Lock-free snapshot reads** via ArcSwap — excellent concurrency design
- **Consistent lock ordering** (writer → KV) across all write paths
- **Domain-separated BLAKE3 hashing** — prevents cross-domain collisions
- **Path traversal protection** — normalize_path handles .. correctly
- **Ed25519 JWT signing** — modern, correct crypto
- **Argon2id API key hashing** — safe defaults
- **404 not 403 for scoped key denials** — no information leakage
- **Per-entry fsync** — individual write durability
- **Fresh file handles for concurrent reads** — correctly fixed the dup() bug
