# CLI Commands

Complete reference for the `aeordb` command-line interface.

## `aeordb start`

Start the AeorDB server.

```bash
aeordb start [OPTIONS]
```

### Flags

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--config` | `-c` | -- | Path to a TOML configuration file |
| `--port` | `-p` | `6830` | TCP port to listen on |
| `--host` | | `0.0.0.0` | Bind address |
| `--database` | `-D` | `data.aeordb` | Path to the `.aeordb` database file |
| `--log-format` | | `pretty` | Log output format: `pretty` or `json` |
| `--auth` | | (none) | Auth provider URI (see below) |
| `--hot-dir` | | (database parent dir) | Directory for write-ahead hot files |
| `--cors-origins` | | (disabled) | CORS allowed origins |
| `--tls-cert` | | -- | Path to TLS certificate PEM file (requires `--tls-key`) |
| `--tls-key` | | -- | Path to TLS private key PEM file (requires `--tls-cert`) |
| `--jwt-expiry` | | `604800` | JWT token lifetime in seconds (7 days) |
| `--chunk-size` | | `262144` | Write chunk size in bytes (256 KiB) |
| `--peers` | | -- | Comma-separated peer URLs to register at startup (persisted, idempotent) |
| `--join` | | -- | URL of an existing cluster member to join (one-shot; fetches the cluster's signing key) |
| `--join-token` | | -- | Root API key (or bearer token) of the existing cluster member, required with `--join` |

`AEORDB_LOG` overrides the default `info` filter with standard
`tracing_subscriber` directives, for example
`AEORDB_LOG=info,aeordb::engine=debug`. Invalid or non-Unicode directives are a
startup error. `aeordb verify` applies the same strict environment override and
exits nonzero rather than silently selecting a different filter.

### Runtime And Lifecycle Configuration Flags

The `start` command generates one explicit option for each of the 41 properties in AeorDB's frozen runtime/lifecycle registry. Run `aeordb start --help` to list the exact options. They include the `memory`, `cache`, `index`, `garbage-collection`, `io`, `query`, `durability`, `maintenance`, `recovery`, `shutdown`, `migration`, and `lifecycle` groups.

Each option takes one explicit value. Command-line values override registered environment variables and stored policy but remain process-local; they are reported as `command_line` and are never written into runtime/lifecycle JSON. The resolver performs type, range, path, and cross-property validation after the database context is known.

```bash
aeordb start -D data.aeordb \
  --memory-hard-limit-bytes 8GiB \
  --cache-index-clean-max-bytes 2GiB \
  --garbage-collection-mark-scratch-max-bytes null \
  --lifecycle-snapshot-writes-enabled false
```

### Auth Modes

The `--auth` flag accepts several formats:

| Value | Mode | Description |
|-------|------|-------------|
| (not set) | Disabled | No authentication required (dev mode) |
| `false`, `null`, `no`, `0` | Disabled | Explicitly disable authentication |
| `self` | Self-contained | AeorDB manages API keys internally |
| `file:///path/to/identity` | File-based | Load identity from a file |

When using `self` mode, the root API key is printed once on first startup. Save it -- it cannot be retrieved again (but can be reset with `emergency-reset`).

### CORS

| Value | Behavior |
|-------|----------|
| (not set) | CORS disabled |
| `*` | Allow all origins |
| `https://a.com,https://b.com` | Allow specific comma-separated origins |

### Examples

```bash
# Development mode (no auth, default port)
aeordb start

# Production with auth on port 8080
aeordb start --port 8080 --database /var/lib/aeordb/prod.aeordb --auth self --log-format json

# Custom hot directory and CORS
aeordb start --database data.aeordb --hot-dir /fast-ssd/hot --cors-origins "*"

# HTTPS with TLS
aeordb start --tls-cert /etc/ssl/cert.pem --tls-key /etc/ssl/key.pem --port 443

# Using a config file
aeordb start --config aeordb.toml

# Config file with CLI overrides
aeordb start --config aeordb.toml --port 8080 --auth false

# Join an existing cluster (one-shot — adopts the cluster's JWT signing key)
aeordb start --database nodeB.aeordb --auth self \
  --join http://nodeA:6830 --join-token "$NODE_A_ROOT_KEY"

# Register additional peers on a node already in a cluster
aeordb start --database data.aeordb --peers "http://nodeC:6830,http://nodeD:6830"

# Show version
aeordb --version
```

### What Happens on Start

1. Binds HTTP and serves `/system/health` with `status: "starting"`
2. Scans the configured emergency-spill locations for unresolved artifacts tied to this database and refuses startup if any are found
3. Opens (or creates) the database file
4. Rebuilds startup state from the WAL if the previous shutdown was dirty
5. Bootstraps root API key (if `--auth self` and no key exists yet)
6. Resets any tasks left in `Running` state from a previous crash to `Pending`
7. Starts background workers:
   - **Heartbeat**: emits clock-sync pulses every 15 seconds
   - **Metrics**: emits system metrics snapshots every 15 seconds
   - **Cron scheduler**: checks protected `/.aeordb-config/cron.json` authority every 60 seconds; manage it through `/system/cron`
   - **Task worker**: dequeues and executes background tasks
   - **Webhook dispatcher**: delivers events to registered webhook URLs
8. Switches the full API router to ready and emits `server_ready` on eligible SSE streams
9. On CTRL+C or SIGTERM, stops accepting new storage work, waits for active work to drain, then flushes buffers

The shutdown drain window defaults to 600 seconds. Set `AEORDB_SHUTDOWN_OPERATION_WAIT_SECS` to override it.

---

## `aeordb deployment-capabilities`

Report machine-readable binary capabilities used by checked installers.

```bash
aeordb deployment-capabilities [--json] [--require CAPABILITY]
```

The current transition-recovery capability is
`aeordb.v3-transition-recovery.v1`. `--require` exits `0` when supported and
`3` when unsupported. Inspection errors exit `1`; an old binary that does not
recognize this command normally exits `2`.

```bash
aeordb deployment-capabilities --json
aeordb deployment-capabilities \
  --require aeordb.v3-transition-recovery.v1
```

---

## `aeordb deployment-check`

Read-only inspection used before replacing a binary that may open an existing
database.

```bash
aeordb deployment-check \
  --database /var/lib/aeordb/data.aeordb \
  [--candidate-capability CAPABILITY] \
  [--json]
```

The check validates bounded v3 header, hot-tail, KV, persistent durability
control, and external spill state without opening the mutable engine. It exits
`0` when replacement is allowed, `3` when policy refuses it, and `1` when
inspection cannot prove a safe answer. Corrupt or unsupported state fails
closed.

A candidate without the transition-recovery capability may proceed only when
the database is quiescent and has no active durability latch, unapplied spill,
or incomplete repair. See [Deployment Safety](../operations/deployment-safety.md)
for the policy matrix and installer behavior.

---

## `aeordb status`

Inspect a running server through the authenticated `GET /system/stats` contract without opening the database file locally.

```bash
aeordb status [--target URL] [--api-key KEY | --token TOKEN] [--json]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--target` | `http://127.0.0.1:6830` | AeorDB HTTP or HTTPS base URL |
| `--api-key` | `AEORDB_ROOT_KEY` when set | Root API key to exchange for a short-lived bearer token |
| `--token` | -- | Existing root bearer token; mutually exclusive with `--api-key` |
| `--json` | `false` | Print the exact structured server response instead of the concise operator view |

The command uses bounded response reads, a five-second connection timeout, and a 30-second request timeout. It rejects HTTP redirects so an API-key request body or bearer token cannot be forwarded to another endpoint. It exits nonzero for invalid targets, credential conflicts, authentication/authorization failures, unreachable servers, oversized responses, malformed JSON, or an incomplete status schema. Credentials are never printed: token-exchange failure bodies are suppressed, and a bearer token reflected by a failed stats endpoint is redacted while non-secret diagnostics remain visible. Prefer `AEORDB_ROOT_KEY` over a command-line key where process listings are visible.

```bash
AEORDB_ROOT_KEY="$ROOT_KEY" aeordb status --target https://files.example.org
aeordb status --target https://files.example.org --token "$TOKEN" --json
```

The human view reports process/coordinator memory, pressure, durability writability/frontier/waiters, repair state, and runtime/lifecycle validity. Use `--json` for per-owner memory, exact configuration sources, spill evidence, and the last completed durability barrier.

---

## `aeordb verify`

Verify database integrity and optionally repair recoverable issues.

```bash
aeordb verify [OPTIONS]
```

### Flags

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--database` | `-D` | `data.aeordb` | Path to the `.aeordb` database file |
| `--repair` | | `false` | Repair recoverable issues |
| `--force-fix-in-place` | | `false` | Apply repairs directly to the original database instead of creating `<database>.repaired` |
| `--yes` | | `false` | Accept emergency-spill replay prompts without interactive confirmation |

### Examples

```bash
# Verify without modifying the database
aeordb verify --database data.aeordb

# Repair into a copy
aeordb verify --repair --database data.aeordb

# Repair the original database in place
aeordb verify --repair --force-fix-in-place --database data.aeordb

# Unattended emergency-spill repair after reviewing the artifacts
aeordb verify --repair --force-fix-in-place --yes --database data.aeordb
```

### Directory Tree Repair

`aeordb verify` reports damaged B-tree directory branches under the Directory Consistency section. Normal read paths can return the readable portion of a damaged B-tree directory, but verification surfaces the missing or corrupt branch so it is not silently hidden.

When `--repair` is used, B-tree directory issues are repaired by first rebuilding only the affected B-tree directory from current live path records. If targeted repair cannot recover the issue, repair falls back to rebuilding the live directory tree from path-key FileRecords.

Each targeted directory publication, full-rebuild directory publication, and
stale directory-locator replacement uses the shared maintenance authority.
If a later repair, final verification, or durability publication fails,
`verify --repair` exits nonzero with a bounded `Partial operation 'verify and
repair'` error. Its `completed` count includes only maintenance actions that
crossed their acknowledgement boundary; nested rebuild and failed-attempt
counts remain explicit in the evidence. A staged Void snapshot is not counted
until the hard hot-tail publication succeeds.

### Storage Accounting

The verification report separates namespace, retention, and physical WAL accounting:

- **Logical data (current HEAD)** is the sum of file sizes reachable from the current HEAD directory tree. Path length, MIME metadata, chunk-list length, and superseded revisions do not inflate it.
- **Retained file-version data** is the logical size of unique live FileRecord versions in the KV index. Canonical `fileid:` identities are counted once; content and path keys are fallback representatives for legacy databases that lack an identity alias. It includes current HEAD versions plus snapshot/fork history, system records outside HEAD, path-safety records, and unreachable versions awaiting garbage collection.
- **Retained outside current HEAD** is retained file-version data minus current HEAD data. It is not synonymous with snapshot history because the retained set has the additional categories above. On a damaged database, both independent measurements remain visible and the subtraction saturates at zero.
- **FileRecord payloads (WAL)** is the serialized FileRecord value volume encountered in the append log. It includes path, identity, and content aliases plus superseded or not-yet-reclaimed entries; it is physical diagnostic data, not user-file size.
- **Chunk payloads (WAL)** is chunk value volume encountered in the append log. **Logical/WAL chunk delta** preserves the legacy current-HEAD-logical minus WAL-chunk calculation, clamped at zero. It is not a current-only deduplication measurement because the WAL side can include retained history and entries awaiting reclamation.

These values do not materialize file content. Current logical bytes come from directory child metadata, while retained-version bytes come from bounded FileRecord metadata reads already performed by verification.

### Emergency Spill Recovery

If startup finds unresolved emergency-spill artifacts for the target database, it exits before serving the normal API and prints the repair command:

```bash
aeordb verify --repair --force-fix-in-place -D /path/to/database.aeordb
```

Repair scans all emergency-spill locations, orders matching artifacts oldest-first, prints the hot-tail and WAL-tail files it found, and prompts before replay. `--yes` skips the prompt for automation. Spill replay must run in place because the artifact marker belongs to the original database path.

If `AEORDB_EMERGENCY_SPILL_DIR` or another process override selected a non-default spill root when the incident was written, supply the same override to both `start` and `verify --repair`. AeorDB automatically scans stored/LKG roots and the platform user-data and temp fallbacks, but it cannot rediscover an arbitrary custom directory after the configuration that named it is removed.

WAL-tail bytes are the only spill payload replayed into the database file. `hot-tail.bin` and `index-buffer.json` are preserved and reported, but repair does not trust them as primary data: after WAL-tail replay it forces a WAL rebuild, reconstructs reusable gaps, and publishes a fresh hot tail.

---

## `aeordb gc`

Run garbage collection to reclaim space from unreachable entries.

```bash
aeordb gc [OPTIONS]
```

### Flags

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--database` | `-D` | `data.aeordb` | Path to the `.aeordb` database file |
| `--dry-run` | | `false` | Report what would be collected without actually deleting |

### Examples

```bash
# Run GC
aeordb gc --database data.aeordb

# Preview what would be collected
aeordb gc --database data.aeordb --dry-run
```

### Output

```
AeorDB Garbage Collection
Database: data.aeordb

Versions scanned: 3
Live entries:     1247
Garbage entries:  89
Reclaimed:        1.2 MB
Duration:         0.3s
```

See [Garbage Collection](../operations/gc.md) for details on the mark-and-sweep algorithm.

---

## `aeordb export`

Export a version as a self-contained `.aeordb` file.

```bash
aeordb export [OPTIONS]
```

### Flags

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--database` | `-D` | `data.aeordb` | Source database file |
| `--output` | `-o` | (required) | Output `.aeordb` file path |
| `--snapshot` | `-s` | (none) | Named snapshot to export |
| `--hash` | | (none) | Specific version hash to export (hex-encoded) |

If neither `--snapshot` nor `--hash` is provided, HEAD is exported.

### Examples

```bash
# Export HEAD
aeordb export --database data.aeordb --output backup.aeordb

# Export a named snapshot
aeordb export --database data.aeordb --output backup-v1.aeordb --snapshot v1

# Export a specific hash
aeordb export --database data.aeordb --output backup.aeordb --hash abc123def456...
```

The output file must not already exist.

The command reports the root hash actually written to the artifact. User-only
exports normally preserve the requested root hash, but an older root that
still names a protected system tree is normalized during export. In that case,
use the reported hash for promotion and identity checks.

See [Backup & Restore](../operations/backup.md) for full backup workflows.

---

## `aeordb diff`

Create a patch `.aeordb` containing only the changeset between two versions.

```bash
aeordb diff [OPTIONS]
```

### Flags

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--database` | `-D` | `data.aeordb` | Source database file |
| `--output` | `-o` | (required) | Output patch file path |
| `--from` | | (required) | Base version (snapshot name or hex hash) |
| `--to` | | HEAD | Target version (snapshot name or hex hash) |

### Examples

```bash
# Diff between two snapshots
aeordb diff --database data.aeordb --output patch.aeordb --from v1 --to v2

# Diff from a snapshot to HEAD
aeordb diff --database data.aeordb --output patch.aeordb --from v1

# Diff between raw hashes
aeordb diff --database data.aeordb --output patch.aeordb --from abc123... --to def456...
```

The `--from` and `--to` arguments first try snapshot name lookup, then fall back to interpreting the value as a hex-encoded hash.

See [Backup & Restore](../operations/backup.md) for incremental backup workflows.

---

## `aeordb import`

Import an export or patch `.aeordb` file into a target database.

```bash
aeordb import [OPTIONS]
```

### Flags

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--database` | `-D` | `data.aeordb` | Target database file |
| `--file` | `-f` | (required) | Backup or patch file to import |
| `--force` | | `false` | Skip base version verification for patches |
| `--promote` | | `false` | Automatically set HEAD to the imported version |

### Examples

```bash
# Import a full backup
aeordb import --database data.aeordb --file backup.aeordb

# Import and promote HEAD
aeordb import --database data.aeordb --file backup.aeordb --promote

# Force-import a patch even if base doesn't match
aeordb import --database data.aeordb --file patch.aeordb --force --promote
```

### Patch Base Verification

When importing a patch (backup_type=2), AeorDB verifies that the target database's HEAD matches the patch's base version. Use `--force` to bypass this check.

See [Backup & Restore](../operations/backup.md) for restore workflows.

---

## `aeordb promote`

Promote a version hash to HEAD.

```bash
aeordb promote [OPTIONS]
```

### Flags

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--database` | `-D` | `data.aeordb` | Database file |
| `--hash` | | (required) | Hex-encoded version hash to promote |

### Examples

```bash
aeordb promote --database data.aeordb --hash abc123def456...
```

The command verifies the hash exists in the database before promoting.

---

## `aeordb stress`

Run stress tests against a running AeorDB instance.

```bash
aeordb stress [OPTIONS]
```

### Flags

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--target` | `-t` | `http://localhost:6830` | Target server URL |
| `--api-key` | `-a` | (required) | API key for authentication |
| `--concurrency` | `-c` | `10` | Number of concurrent workers |
| `--duration` | `-d` | `10s` | Test duration (e.g., `30s`, `5m`) |
| `--operation` | `-o` | `mixed` | Operation type: `write`, `read`, or `mixed` |
| `--file-size` | `-s` | `1kb` | File size for writes (e.g., `512b`, `1kb`, `1mb`) |
| `--path-prefix` | `-p` | `/stress-test` | Path prefix for stress test files |

### Examples

```bash
# Quick mixed read/write test
aeordb stress --api-key $API_KEY

# Heavy write test for 5 minutes
aeordb stress --api-key $API_KEY --operation write --concurrency 50 --duration 5m --file-size 10kb

# Read-only test against production
aeordb stress --target https://prod.example.com --api-key $API_KEY --operation read --concurrency 100 --duration 30s
```

---

## `aeordb emergency-reset`

Revoke the current root API key and generate a new one. Use this if the root key is lost or compromised.

```bash
aeordb emergency-reset [OPTIONS]
```

### Flags

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--database` | `-D` | (required) | Database file |
| `--force` | | `false` | Skip confirmation prompt |

### Examples

```bash
# Interactive (prompts for confirmation)
aeordb emergency-reset --database data.aeordb

# Non-interactive
aeordb emergency-reset --database data.aeordb --force
```

### What Happens

1. Finds all API keys linked to the root user (nil UUID)
2. Revokes each one
3. Generates a new root API key
4. Prints the new key (shown once, save it immediately)

```
WARNING: This will invalidate the current root API key.
A new root API key will be generated.
Proceed? [y/N]: y
Revoked 1 existing root API key(s).

==========================================================
  NEW ROOT API KEY (shown once, save it now!):
  aeordb_abc123def456...
==========================================================
```

This command requires direct file access to the database -- it cannot be run over HTTP. It is intended for recovery scenarios where you have lost the root API key.

---

## See Also

- [Garbage Collection](../operations/gc.md) -- GC algorithm details
- [Backup & Restore](../operations/backup.md) -- backup workflows
- [Task System & Cron](../operations/tasks.md) -- background tasks and scheduling
- [Reindexing](../operations/reindex.md) -- reindex process details
