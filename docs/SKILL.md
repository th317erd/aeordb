---
name: aeordb-operations
description: Use when working with AeorDB databases, running AeorDB locally or in production, investigating AeorDB corruption, resetting auth keys, repairing databases, or changing scripts that start or stop AeorDB. This skill explains corruption evidence preservation and the correct AeorDB startup and graceful shutdown procedures.
---

# AeorDB Operations Safety

AeorDB is a single-file database with an append log, KV pages, a hot tail crash-recovery journal, and an explicit storage-engine shutdown path. Treat it like a database, not like a disposable web server.

## Prime Directive: Corruption Is Valuable Evidence

Any and all AeorDB corruption events are **valuable**.

Do **not** destroy evidence.

If corruption is suspected, preserve the original artifact before doing anything that might mutate it. A corrupt dev database is often more valuable than a working replacement because the database team can inspect the exact bytes, logs, and runtime sequence that produced the failure.

### Corruption Signals

Treat these as evidence events:

- AeorDB logs mention corruption, invalid hash, invalid header, invalid hot tail, invalid KV/NVT, dirty startup, failed rebuild, or storage-engine errors.
- HTTP requests fail with unexpected 500s from AeorDB.
- Auth/API-key lookup fails with storage errors.
- Startup reports recovery, rebuild, dirty startup, or lock trouble.
- A database requires `emergency-reset`, `verify --repair`, or manual intervention.
- AeorDB was stopped with `SIGKILL`, crashed, power-cycled, or was killed by tooling.

### Evidence Preservation Procedure

Before any reset, repair, GC, import/export, migration, or repeated restart against the same suspect DB:

1. Stop AeorDB gracefully if it is running.
2. Copy the database file, lock file if present, hot/log side files if present, and AeorDB logs to an evidence directory.
3. Record the timestamp, command being run, observed error, AeorDB version, process IDs, database path, and log path.
4. Work from a copy or a fresh database.
5. Ask the user before mutating the preserved specimen.

Suggested evidence directory:

```bash
mkdir -p /tmp/codex/aeordb-evidence/$(date -u +%Y%m%dT%H%M%SZ)
```

Suggested files to preserve:

```bash
cp -a /path/to/data.aeordb* /tmp/codex/aeordb-evidence/<timestamp>/
cp -a /path/to/aeordb.log /tmp/codex/aeordb-evidence/<timestamp>/ 2>/dev/null || true
```

Use read-only inspection first. Do not run `verify --repair`, `emergency-reset`, GC, import, export-over, compaction, or any command that writes to the suspect file until the original has been preserved.

## Correct Startup

Use `aeordb start` with an explicit database path. Do not rely on whichever default path the shell happens to use.

Development example:

```bash
mkdir -p .aeordb /tmp/codex/aeordb
AEORDB_LOG=aeordb=debug,aeordb_cli=info \
AEORDB_LOG_MAGIC_LINKS=1 \
aeordb start \
  -D .aeordb/dev.aeordb \
  --host 127.0.0.1 \
  --port 6830 \
  --auth self \
  2>&1 | tee /tmp/codex/aeordb/aeordb.log
```

Operational rules:

- Save the root API key printed on first `--auth self` startup. It is shown once.
- Keep the database path, log path, port, auth mode, and root key in the project's ignored dev environment file when appropriate.
- Start only one AeorDB process per database file.
- If the port is occupied, identify the existing process and shut it down gracefully rather than starting a second process against the same DB.
- If startup reports corruption or dirty recovery, preserve the database and logs before attempting fixes.

## Correct Shutdown

AeorDB handles Ctrl+C/SIGINT and SIGTERM. Use one of those and wait for the process to exit.

Preferred interactive shutdown:

```text
Ctrl+C
```

Preferred scripted shutdown:

```bash
kill -TERM "$AEORDB_PID"
wait "$AEORDB_PID"
```

If the process is not a child of the shell, send SIGTERM and poll until it exits:

```bash
kill -TERM "$AEORDB_PID"
while kill -0 "$AEORDB_PID" 2>/dev/null; do
  sleep 0.2
done
```

What graceful shutdown does:

1. Cancels server/background workers.
2. Calls `StorageEngine::shutdown()`.
3. Flushes KV write-buffer entries to disk pages.
4. Flushes the hot tail crash-recovery journal.
5. Updates the database header with `hot_tail_offset` and `entry_count`.
6. Calls `sync_all()` so OS-buffered writes are durable.
7. Prints/logs graceful shutdown completion.

Never use `kill -9`, `SIGKILL`, `pkill -9`, process-manager hard-kill, or terminal-session destruction as the normal shutdown path. Those skip AeorDB's shutdown procedure and can destroy the exact recovery state the database team needs to inspect.

## Forbidden Until Evidence Is Preserved

Do not run these against a suspected corrupt original:

- `aeordb emergency-reset`
- `aeordb verify --repair`
- `aeordb gc`
- imports, compaction, migration, or any command that rewrites the DB
- repeated restart attempts against the same suspect file
- deletion of `.aeordb/`, `.lock`, hot files, or logs

After preservation, destructive recovery can be done on the live copy only if the user agrees.

## Recovery Workflow

When AeorDB looks broken:

1. Stop gracefully with SIGINT/SIGTERM and wait.
2. Preserve the database and logs.
3. Start a fresh replacement DB or copy the specimen and work on the copy.
4. Only then use `emergency-reset`, repair, GC, or other mutating commands.
5. Keep notes about what was preserved and what was changed.

If the user asks whether the corrupt DB is still available, answer based on actual file checks. Do not assume backups exist.

