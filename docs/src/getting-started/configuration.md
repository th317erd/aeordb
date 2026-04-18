# Configuration

AeorDB is configured through CLI flags at startup and through configuration files stored inside the database itself.

## CLI Flags

```bash
aeordb start [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--port`, `-p` | `3000` | HTTP listen port |
| `--database`, `-D` | `data.aeordb` | Path to the database file (created if it does not exist) |
| `--auth` | self-contained | Auth provider URI (see [Auth Modes](#auth-modes)) |
| `--hot-dir` | database parent dir | Directory for write-ahead hot files (crash recovery journal) |
| `--cors-origins` | disabled | CORS allowed origins (see [CORS](#cors)) |
| `--log-format` | `pretty` | Log output format: `pretty` or `json` |

### Examples

```bash
# Minimal: local development with no auth
aeordb start --database dev.aeordb --port 8080 --auth false

# Production: custom port, explicit hot directory, CORS for your frontend
aeordb start \
  --database /var/lib/aeordb/prod.aeordb \
  --port 443 \
  --hot-dir /var/lib/aeordb/hot \
  --cors-origins "https://myapp.com,https://admin.myapp.com" \
  --log-format json
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
curl -X POST http://localhost:3000/auth/token \
  -H "Content-Type: application/json" \
  -d '{"api_key": "aeor_ak_7f3b2a1c..."}'

# Use the token for subsequent requests
curl http://localhost:3000/files/users/ \
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
curl -X PUT http://localhost:3000/files/.config/cors.json \
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

Indexes are configured per-directory by storing a `.config/indexes.json` file under the directory path. When this file changes, the engine automatically triggers a background reindex of all files in that directory.

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

Schedule recurring background tasks by storing `/.config/cron.json`:

```bash
curl -X PUT http://localhost:3000/files/.config/cron.json \
  -H "Content-Type: application/json" \
  -d '{
    "schedules": [
      {
        "id": "weekly-gc",
        "task_type": "gc",
        "schedule": "0 3 * * 0",
        "args": {},
        "enabled": true
      },
      {
        "id": "nightly-reindex",
        "task_type": "reindex",
        "schedule": "0 2 * * *",
        "args": {"path": "/data/"},
        "enabled": true
      }
    ]
  }'
```

The `schedule` field uses standard 5-field cron syntax: `minute hour day_of_month month day_of_week`. Cron schedules can also be managed via the HTTP API at `/system/cron`.

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
