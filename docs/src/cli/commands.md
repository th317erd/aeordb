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
| `--jwt-expiry` | | `3600` | JWT token lifetime in seconds |
| `--chunk-size` | | `262144` | Write chunk size in bytes (256 KiB) |

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

# Show version
aeordb --version
```

### What Happens on Start

1. Opens (or creates) the database file
2. Bootstraps root API key (if `--auth self` and no key exists yet)
3. Resets any tasks left in `Running` state from a previous crash to `Pending`
4. Starts background workers:
   - **Heartbeat**: emits clock-sync pulses every 15 seconds
   - **Metrics**: emits system metrics snapshots every 15 seconds
   - **Cron scheduler**: checks `/.config/cron.json` every 60 seconds
   - **Task worker**: dequeues and executes background tasks
   - **Webhook dispatcher**: delivers events to registered webhook URLs
5. Binds to the TCP port and begins serving requests
6. Shuts down gracefully on CTRL+C

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
