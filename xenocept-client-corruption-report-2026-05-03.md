# Xenocept Client — Database Corruption Report

**Date:** 2026-05-03
**Reporter:** xenocept-client development team
**AeorDB Version:** 0.9.5 (git main @ 3a294da7)

---

## Summary

During xenocept-client development, three `.aeordb` files were backed up as "corrupt" by the client's `Store::open_or_create` logic. **None of these files are actually corrupt** — they were locked by a previous running instance. The client code (modeled after aeordb-client) treats a lock error the same as file corruption, which is incorrect.

## Corrupt Backup Files

All files are located at `/home/wyatt/.local/share/xenocept/`:

| File | Timestamp | Size | Cause |
|------|-----------|------|-------|
| `xenocept.aeordb.corrupt.1777683711` | 2026-05-01 16:00 | 71,444 bytes | Lock from previous instance after `pkill -9` |
| `xenocept.aeordb.corrupt.1777848590` | 2026-05-03 14:53 | 71,444 bytes | Lock from previous instance after `pkill -9` |
| `xenocept.aeordb.corrupt.1777864516` | 2026-05-03 19:59 | 71,444 bytes | Lock from previous instance after `pkill -9` |

The current active database:
- `/home/wyatt/.local/share/xenocept/xenocept.aeordb` — 659,878 bytes (2026-05-03 20:58)

## Root Cause

The xenocept-client uses a singleton process model with file-lock takeover (modeled after aeordb-client). When a new instance starts and the old one is still running:

1. The new instance acquires the **process lock** (via `fs2::FileExt::try_lock_exclusive`)
2. The new instance attempts to open the **AeorDB database**
3. AeorDB returns: `IO error: Database '...' is locked by another process`
4. The client code interprets this as corruption → backs up the file → creates a fresh database
5. **Data from the previous session is lost** (moved to `.corrupt` backup)

The issue is in step 4: a "locked" error is not corruption. The database is perfectly healthy — it's just still held by the dying process.

## Timeline of a Typical Occurrence

```
T+0.0s  New instance starts, sends SIGTERM to old instance via /api/v1/shutdown
T+0.1s  New instance acquires the process lock (.lock file)
T+0.2s  New instance tries StorageEngine::open() → FAILS (AeorDB lock still held)
T+0.3s  Client backs up DB as .corrupt, creates fresh DB
T+2.0s  Old instance finally exits, AeorDB lock released (too late)
```

## Questions for Server Team

1. **Lock error vs corruption:** Is there a way to distinguish "database is locked by another process" from "database file is actually corrupted" in the `StorageEngine::open()` error type? Currently both return generic IO errors. A typed error (e.g., `EngineError::Locked` vs `EngineError::Corrupted`) would let the client handle them differently.

2. **Lock file stale detection:** When a process is killed with `SIGKILL` (signal 9), the AeorDB lock file (`xenocept.aeordb.lock`) is never released. Is there a way to detect a stale lock (e.g., check if the PID that holds it is still alive)?

3. **Graceful shutdown timing:** `StorageEngine::shutdown()` is now called explicitly on all exit paths. However, there's a timing gap between the process lock release and the AeorDB lock release. Is there a way to release the AeorDB lock as the first step of `shutdown()`, before flushing?

4. **Recovery of "corrupt" files:** These three `.corrupt` files should be valid AeorDB databases. Can you confirm they're intact? The server team may want to inspect them to verify the entry scanner can read them cleanly.

## Current Mitigations

The client now has:
- Explicit `StorageEngine::shutdown()` calls on all exit paths (tray quit, API shutdown, post-Tauri exit)
- A retry loop (10 attempts, 500ms each) when `StorageEngine::open()` fails after takeover
- Graceful shutdown via HTTP API before falling back to SIGTERM

## Recommended Fix (Client Side)

The client should NOT treat a lock error as corruption. Instead:
1. If the error message contains "locked by another process", retry (already doing this)
2. Only back up and recreate if the error is an actual file corruption (bad magic bytes, truncated header, etc.)
3. Never rename a locked file — the lock holder still needs it

## Recommended Fix (Server/Engine Side)

1. Return a typed error from `StorageEngine::open()` that distinguishes lock errors from corruption
2. Consider making the `Drop` impl call `shutdown()` (the server team already identified this gap)
3. Consider adding a `force_open()` or `open_ignore_lock()` for recovery scenarios
