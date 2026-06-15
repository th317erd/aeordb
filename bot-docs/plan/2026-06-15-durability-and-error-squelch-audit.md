# Durability and Error-Squelch Audit - 2026-06-15

## Fixed in first pass

- Storage transaction completion now returns `EngineResult<()>` and records a latched durability failure when WAL sync, hot-tail flush, or header update fails.
- Hot-tail timer flush now stops after failed WAL sync or hot-tail flush instead of publishing a header checkpoint that may reference non-durable data.
- Shutdown now attempts all flush/sync steps, but returns `DurabilityFailure` and does not set `shutdown_complete` if any critical flush/sync fails.
- Engine health now reports latched durability failures internally, and public `/system/health` folds that state into the top-level status.
- The hot-tail clear/update after KV page flush now truncates and syncs the hot-tail write.
- Initial hot-tail creation now propagates and syncs errors instead of ignoring the write result.
- Backup export, CLI export, and patch creation now publish through a durable rename helper that fsyncs the parent directory on Unix.
- ZIP download generation now fails the request on ZIP write errors instead of returning a partial archive as success.
- Peer sync apply now fails the sync cycle on local chunk/file/symlink write errors, conflict-store errors, and sync-state persistence errors.
- GC now aborts when it cannot create the pre-GC safety snapshot, or when the post-sweep hot-tail flush fails.
- `StorageEngine::drop` now logs shutdown failures instead of discarding them silently.

## Remaining high-priority audit items

- Add a real Windows implementation for parent-directory sync in `engine::durability`; the helper currently centralizes the call site but no-ops on Windows.
- Add a test/failpoint harness for forced sync failures so shutdown, transaction, timer, GC, export, and sync-state failure behavior is directly asserted.
- Decide whether a latched durability failure should reject all subsequent writes immediately. The health state is now visible, but write gating is still a follow-up policy decision.
- Revisit `DiskKVStore::drop`: it logs failed flushes but cannot return. This is acceptable only if every normal owner path calls `StorageEngine::shutdown()` and handles the result.
- Revisit index flush failure policy. Indexes are rebuildable, but API responses should not imply indexing succeeded when index flush failed.

## Medium-priority squelch findings

- `task_worker` ignores reindex checkpoint update failures. Risk: repeated work or stale progress after restart, not direct data loss.
- `start` ignores failures to enqueue initial reindex and task status reset. Risk: missing background maintenance, not direct write corruption.
- `system_store` cleanup/delete helpers ignore some delete failures. Risk depends on caller; likely stale system metadata.
- `tree_walker` ignores subtree walk errors for some system paths. Risk: exports/sync views may be incomplete without obvious failure.
- `conflict_store` ignores delete failures for old metadata while replacing conflict records. Risk: stale conflict sidecar records.
- `backup_routes` ignores temp-file cleanup failures. Risk: orphaned temp files, not corrupt final output.

## Low-priority or acceptable ignores

- Best-effort temp cleanup after an already-failed operation.
- Channel send failures where "no receiver" is explicitly acceptable.
- Optional parser fallbacks and `Option` conversions in native parsers/query parsing.
- Stress/soak metrics flushes where lost telemetry is acceptable and does not affect database state.
