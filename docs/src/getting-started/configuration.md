# Configuration

AeorDB server settings come from CLI flags and an optional TOML file. Engine runtime and lifecycle policy comes from strict configuration documents stored inside the database, registered environment variables, and the corresponding `aeordb start` flags.

## Configuration File

AeorDB supports a TOML configuration file for all server settings. Pass it with `--config`:

```bash
aeordb start --config aeordb.toml
```

Every TOML server key has a corresponding CLI flag. CLI flags override TOML values. Omit any key to use the built-in default.

```toml
[server]
port = 6830
host = "0.0.0.0"
log_format = "pretty"

[server.tls]
cert = "/path/to/cert.pem"
key = "/path/to/key.pem"

[server.cors]
origins = ["https://app.example.com"]

[auth]
mode = "self"
jwt_expiry_seconds = 3600

[storage]
database = "data.aeordb"
chunk_size = 262144
# hot_dir = "./hot" # Legacy compatibility only; current engine ignores it.
```

An example config file is included in the repository as `aeordb.example.toml`.

## CLI Flags

```bash
aeordb start [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--config`, `-c` | — | Path to a TOML configuration file |
| `--port`, `-p` | `6830` | HTTP listen port |
| `--host` | `0.0.0.0` | Bind address |
| `--database`, `-D` | `data.aeordb` | Path to the database file (created if it does not exist) |
| `--auth` | self-contained | Auth provider URI (see [Auth Modes](#auth-modes)) |
| `--hot-dir` | database parent dir | Legacy compatibility value; accepted but unused by the current in-file hot-tail engine |
| `--cors-origins` | disabled | CORS allowed origins (see [CORS](#cors)) |
| `--log-format` | `pretty` | Log output format: `pretty` or `json` |
| `--tls-cert` | — | Path to TLS certificate PEM file (requires `--tls-key`) |
| `--tls-key` | — | Path to TLS private key PEM file (requires `--tls-cert`) |
| `--jwt-expiry` | `604800` | JWT token lifetime in seconds (7 days) |
| `--chunk-size` | `262144` | Write chunk size in bytes (256 KiB default) |

### Legacy Hot-dir Compatibility

The `--hot-dir` flag and `storage.hot_dir` TOML key predate the single-file
storage layout. They remain accepted for legacy compatibility, but the current
engine ignores the value and stores crash-recovery state in the `.aeordb`
file's hot tail. New configurations should omit it.

### Logging Filters

AeorDB logs at `info` by default. Set `AEORDB_LOG` to a
[`tracing_subscriber` filter directive](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
to change global or module-specific verbosity:

```bash
AEORDB_LOG=info,aeordb::engine=debug aeordb start -D data.aeordb
```

When present, `AEORDB_LOG` takes precedence over the configured default log
level. The value must be valid Unicode and every directive must parse. AeorDB
rejects `start` and diagnostic commands such as `verify` when the filter is
malformed; it does not silently fall back to another level. Logging also uses
one process-wide subscriber, so embedded callers that install their own
subscriber should use the library's fallible initialization API and handle an
initialization conflict explicitly.

### Runtime And Lifecycle Overrides

`aeordb start --help` exposes one explicit flag for every property in the frozen runtime/lifecycle registry. These flags use the same value syntax as registered environment variables and have the highest precedence:

1. command-line override;
2. registered environment override;
3. valid stored runtime/lifecycle document;
4. validated last-known-good policy when applicable; then
5. compiled default.

Process overrides are ephemeral. AeorDB reports their source as `command_line` through `/system/runtime` or `/system/lifecycle`, but never copies them into the stored JSON document.

The same complete configuration envelopes are embedded in root `/system/stats`, root-only Prometheus source gauges, the root-only `metrics` SSE pulse, `aeordb status --json`, and the Dashboard. Non-root stats callers retain operational values but receive explicit redaction markers for registered root-only paths. This keeps source, validity, degradation, pending-restart, and pending-convergence reporting consistent across every observability surface.

```bash
aeordb start -D data.aeordb \
  --memory-hard-limit-bytes 8GiB \
  --index-flush-after-seconds 30 \
  --lifecycle-snapshot-writes-enabled false
```

Boolean flags require an explicit `true` or `false`. Byte quantities accept exact binary units such as `MiB`, `GiB`, and `TiB`; count/time values are unsigned integers; optional byte values accept `null`; and path values must be absolute. Unknown flags, missing values, and duplicate flags are rejected. See [`aeordb start`](../cli/commands.md#aeordb-start) for the command surface and [Admin API](../api/admin.md#runtime-configuration) for stored policy and status.

### TLS

AeorDB supports native HTTPS via rustls. Provide both a certificate and private key:

```bash
aeordb start \
  --tls-cert /etc/ssl/certs/aeordb.pem \
  --tls-key /etc/ssl/private/aeordb.key \
  --port 443
```

Both `--tls-cert` and `--tls-key` must be provided together -- supplying only one is an error. When TLS is configured, the server listens for HTTPS connections instead of plain HTTP.

### Examples

```bash
# Minimal: local development with no auth
aeordb start --database dev.aeordb --port 8080 --auth false

# Production: HTTPS with auth, CORS for your frontend
aeordb start \
  --database /var/lib/aeordb/prod.aeordb \
  --port 443 \
  --tls-cert /etc/ssl/certs/aeordb.pem \
  --tls-key /etc/ssl/private/aeordb.key \
  --cors-origins "https://myapp.com,https://admin.myapp.com" \
  --log-format json

# Using a config file with CLI overrides
aeordb start --config aeordb.toml --port 8080
```

## Auth Modes

The `--auth` flag controls how authentication works:

| Value | Behavior |
|-------|----------|
| `false` (or `null`, `no`, `0`) | Auth disabled -- all requests are allowed without tokens. Use for local development only. |
| *(omitted)* | Self-contained mode (default). AeorDB manages its own users, API keys, and JWT tokens. A root API key is printed on first startup. |
| `file:///path/to/identity.json` | External identity file. AeorDB loads cryptographic keys from the specified file. A bootstrap API key is generated on first use. |

### Self-Contained Auth (Default)

On first startup, AeorDB creates an internal identity store and prints a root API key:

```
Root API key: aeor_ak_7f3b2a1c...
```

Use this key to create additional users and API keys via the admin API. If you lose the root key, use `aeordb emergency-reset` to generate a new one.

### Obtaining a JWT Token

```bash
# Exchange API key for a JWT token
curl -X POST http://localhost:6830/auth/token \
  -H "Content-Type: application/json" \
  -d '{"api_key": "aeor_ak_7f3b2a1c..."}'

# Use the token for subsequent requests
curl http://localhost:6830/files/users/ \
  -H "Authorization: Bearer eyJhbG..."
```

## CORS

### Global CORS via CLI

The `--cors-origins` flag sets allowed origins for all routes:

```bash
# Allow all origins
aeordb start --cors-origins "*"

# Allow specific origins (comma-separated)
aeordb start --cors-origins "https://myapp.com,https://admin.myapp.com"
```

Without `--cors-origins`, no CORS headers are sent and cross-origin browser requests will fail.

### Per-Path CORS

For fine-grained control, store a `/.config/cors.json` file in the database:

```bash
curl -X PUT http://localhost:6830/files/.config/cors.json \
  -H "Content-Type: application/json" \
  -d '{
    "rules": [
      {
        "path": "/public/",
        "origins": ["*"],
        "methods": ["GET", "HEAD"],
        "headers": ["Content-Type"]
      },
      {
        "path": "/api/",
        "origins": ["https://myapp.com"],
        "methods": ["GET", "POST", "PUT", "DELETE", "HEAD", "OPTIONS"],
        "headers": ["Content-Type", "Authorization"]
      }
    ]
  }'
```

Per-path rules are checked first. If no rule matches the request path, the global `--cors-origins` setting applies.

## Index Configuration

Indexes are configured per-directory by storing a `.aeordb-config/indexes.json` file under the directory path. When this file changes, the engine automatically triggers a background reindex of all files in that directory.

```json
{
  "indexes": [
    {"name": "title", "type": "string"},
    {"name": "age", "type": "u64"},
    {"name": "email", "type": ["string", "trigram"]},
    {"name": "created", "type": "timestamp"}
  ]
}
```

### Index Types

| Type | Description | Use Case |
|------|-------------|----------|
| `u64` | Unsigned 64-bit integer | Counts, IDs, sizes |
| `i64` | Signed 64-bit integer | Temperatures, offsets, balances |
| `f64` | 64-bit floating point | Coordinates, measurements, scores |
| `string` | Exact string match | Categories, statuses, enum values |
| `timestamp` | UTC millisecond timestamp | Date ranges, temporal queries |
| `trigram` | Trigram-based fuzzy text | Typo-tolerant search, substring matching |
| `phonetic` | Phonetic matching (Soundex) | Name search ("Smith" matches "Smyth") |
| `soundex` | Soundex encoding | Alternative phonetic matching |
| `dmetaphone` | Double Metaphone | Multi-cultural phonetic matching |

### Multi-Strategy Indexes

A single field can have multiple index types. Specify `type` as an array:

```json
{"name": "title", "type": ["string", "trigram", "phonetic"]}
```

This creates three index files (`title.string.idx`, `title.trigram.idx`, `title.phonetic.idx`) from the same source field. Use the appropriate query operator to target each index type.

### Source Resolution

By default, the index `name` is used as the JSON field name. For nested fields or parser output, use the `source` array:

```json
{
  "parser": "pdf-extractor",
  "indexes": [
    {"name": "title", "source": ["metadata", "title"], "type": "string"},
    {"name": "author", "source": ["metadata", "author"], "type": ["string", "trigram"]},
    {"name": "page_count", "source": ["metadata", "pages"], "type": "u64"}
  ]
}
```

See [Indexing & Queries](../concepts/indexing.md) for the full indexing reference.

## Cron Configuration

Schedule recurring background tasks through the root-only cron API. Repeat this
request for each schedule:

```bash
curl -X POST http://localhost:6830/system/cron \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "id": "weekly-gc",
    "task_type": "gc",
    "schedule": "0 3 * * 0",
    "args": {},
    "enabled": true
  }'
```

Supported task types: `"gc"`, `"reindex"`, and `"backup"`. The `"backup"` type accepts `backup_dir`, `retention_count`, and `snapshot` arguments (see [Backup & Restore](../operations/backup.md#automated-backup-scheduling)).

The `schedule` field uses standard 5-field cron syntax: `minute hour day_of_month month day_of_week`. AeorDB stores the complete document at the protected `/.aeordb-config/cron.json` path; use `GET`, `POST`, `PATCH`, and `DELETE` under `/system/cron` rather than generic file routes.

## Lifecycle Policy

Database lifecycle policy is stored in the database at `/.aeordb-config/lifecycle.json` and can be managed through the root-only `/system/lifecycle` API.

```bash
curl -X PUT http://localhost:6830/system/lifecycle \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "schema_version": 1,
    "snapshot_writes_enabled": false,
    "snapshot_retention": {
      "auto_months": 1,
      "manual_months": 12
    },
    "garbage_collection": {
      "pending_delete_grace_seconds": 86400
    }
  }'
```

`snapshot_writes_enabled` defaults to `true`. Set it to `false` to disallow new snapshot writes without removing or breaking existing snapshots. Existing snapshots can still be listed, read, restored, deleted, exported, and pruned. Automatic safety snapshots are skipped while this flag is disabled.

Both `/system/lifecycle` and `/system/runtime` support root-only GET, full-replacement PUT, and RFC 7396 PATCH. GET and successful writes return the complete effective configuration plus per-property sources, stored/LKG status, degradation, and pending activation. PUT requires `Content-Type: application/json`; PATCH accepts `application/merge-patch+json` or `application/json`. Both use strict duplicate-aware validation. Process environment and command-line overrides remain ephemeral and are never copied into the stored JSON. Use PATCH for a partial policy change:

```bash
curl -X PATCH http://localhost:6830/system/lifecycle \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"snapshot_retention":{"auto_months":3}}'
```

PATCH requires a valid current document or validated last-known-good policy. On a database with no stored document, initialize it with PUT first. The engine-owned `/.aeordb-config/runtime.json` and `/.aeordb-config/lifecycle.json` paths are deliberately inaccessible through generic file and blob routes.

## Compression

AeorDB uses zstd compression automatically when configured. To enable compression for a directory, add the `compression` field to the index config:

```json
{
  "compression": "zstd",
  "indexes": [
    {"name": "title", "type": "string"}
  ]
}
```

### Auto-Detection

When compression is enabled, the engine applies heuristics to decide whether to actually compress each file:

- **Files smaller than 500 bytes** are stored uncompressed (header overhead negates savings)
- **Already-compressed formats** (JPEG, PNG, MP4, ZIP, etc.) are stored uncompressed
- **Text, JSON, XML, and other compressible types** are compressed with zstd

Compression is transparent -- reads automatically decompress. The content hash is always computed on the raw uncompressed data, so deduplication works regardless of compression settings.
