# Logging System Plan

## Principles

1. **Every log line is structured JSON** (production) or pretty-printed (development)
2. **Every log line carries a request_id** — one UUID per HTTP request, propagated through all layers
3. **Spans show durations** — not just events. Nested spans for request → path resolution → chunk read
4. **Filtering is granular** — per-module, per-level, via environment variable
5. **Near-zero cost when disabled** — tracing's subscriber model ensures this
6. **No sensitive data in logs** — API keys, JWT tokens, and file contents are NEVER logged
7. **High-resolution timestamps** — microsecond precision on every entry

## Log Levels (with meaning)

| Level | When to use | Examples |
|---|---|---|
| ERROR | Something broke. Needs human attention. | Chunk integrity failure, redb transaction failed, WASM trap |
| WARN | Concerning but not broken. | Rate limit hit, auth failure, deprecated API usage, slow query |
| INFO | Normal operations worth recording. | Request completed, file stored, version created, server started |
| DEBUG | Detailed operational info for troubleshooting. | Path resolution steps, chunk reads/writes, B-tree operations |
| TRACE | Extremely detailed. Development only. | Every chunk hash, byte counts, intermediate states, serialization |

## Architecture

### 1. Subscriber Configuration (src/logging/mod.rs)

```rust
pub struct LogConfig {
  pub format: LogFormat,         // Json or Pretty
  pub level: String,             // default "info", overridden by AEORDB_LOG env var
  pub show_target: bool,         // show module path (aeordb::storage::chunk_store)
  pub show_thread: bool,         // show thread name/id
  pub show_file_line: bool,      // show source file:line (debug/dev only)
}

pub enum LogFormat {
  Json,     // production: machine-parseable, one JSON object per line
  Pretty,   // development: human-readable, colored, multi-line spans
}
```

Initialize with:
```rust
pub fn initialize_logging(config: &LogConfig) -> Result<()>;
```

Uses `tracing_subscriber::fmt` with:
- `EnvFilter` from `AEORDB_LOG` env var (falls back to config level)
- `.json()` or `.pretty()` based on format
- `.with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())` for high-res timestamps
- `.with_current_span(true)` for span context in every event

### 2. Request ID Middleware (src/logging/request_id.rs)

A tower middleware that:
1. Generates a UUID v4 for each incoming request
2. Creates a tracing span with `request_id = %id`
3. Adds `X-Request-Id` response header
4. All downstream log events automatically inherit the request_id from the span

```rust
// In a handler, this "just works":
tracing::info!(path = %path, "File stored");
// Output includes request_id from the parent span automatically
```

If the client sends an `X-Request-Id` header, USE that instead of generating one (correlation with upstream services).

### 3. Instrumentation Points

#### HTTP Layer
- Request received: INFO with method, path, content_length
- Request completed: INFO with method, path, status, duration_ms
- Request error: ERROR with method, path, error, status

#### Auth
- JWT validation success: DEBUG with user_id (NOT the token)
- JWT validation failure: WARN with reason (expired, invalid signature, etc.)
- API key exchange: INFO with key_id (NOT the key itself)
- Rate limit hit: WARN with key/IP, limit, window

#### Filesystem
- Path resolution: DEBUG span wrapping the full resolution, with path and depth
- Each segment lookup: TRACE with segment name, table name, result
- File store: INFO with path, size, chunk_count, duration_ms
- File read: INFO with path, size, chunk_count (at stream start)
- File delete: INFO with path
- Directory created (mkdir-p): DEBUG with path
- Directory list: DEBUG with path, entry_count

#### Chunk Store
- Chunk write: TRACE with hash (hex, first 16 chars), size
- Chunk read: TRACE with hash (hex, first 16 chars), size
- Chunk dedup: DEBUG with hash — "chunk already exists, skipped write"
- Integrity failure: ERROR with hash, expected vs actual

#### Plugins
- Plugin invocation: INFO with path, plugin_type, in a span for duration
- Plugin error: ERROR with path, error_type, message
- WASM fuel exhausted: WARN with path, fuel_consumed

#### Versions
- Version created: INFO with name, savepoint_id
- Version restored: WARN with name (WARN because it's destructive)
- Version deleted: INFO with name

### 4. Sensitive Data Policy

NEVER log:
- JWT tokens (log user_id extracted from claims instead)
- API key values (log key_id prefix instead)
- Magic link codes
- Refresh tokens
- File content/data bytes
- Request/response bodies (except size)

ALWAYS log:
- Request IDs
- User IDs (from JWT claims)
- Path being accessed
- Operation being performed
- Error details and context
- Timing information

### 5. Output Examples

**JSON (production):**
```json
{"timestamp":"2026-03-28T15:04:05.123456Z","level":"INFO","target":"aeordb::server::routes","request_id":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","message":"File stored","path":"/myapp/users/alice.json","size":1234,"chunk_count":1,"duration_ms":2.3}
```

**Pretty (development):**
```
2026-03-28T15:04:05.123Z  INFO aeordb::server::routes{request_id=a1b2c3d4}:
  File stored path="/myapp/users/alice.json" size=1234 chunk_count=1 duration_ms=2.3
```

### 6. Configuration

**Environment variable:** `AEORDB_LOG`
```bash
# Default: info for everything
AEORDB_LOG=info

# Debug storage, info everything else
AEORDB_LOG=info,aeordb::storage=debug

# Trace chunk operations specifically
AEORDB_LOG=info,aeordb::storage::chunk_store=trace

# Quiet mode: errors only
AEORDB_LOG=error
```

**CLI flag:** `--log-level info` and `--log-format json|pretty`

## Dependencies

```toml
# Already present:
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }

# Add:
tracing-test = "0.2"  # dev-dependency for testing log output
```

Note: the "json" feature needs to be added to tracing-subscriber's features list.

## Tests

```
spec/logging/
  logging_spec.rs
    - test_request_id_generated_for_each_request
    - test_request_id_appears_in_response_header
    - test_client_request_id_preserved
    - test_log_format_json_is_valid_json
    - test_log_level_filtering_works
    - test_info_level_does_not_emit_debug
    - test_error_includes_context
    - test_api_key_not_logged (sensitive data check)
    - test_jwt_token_not_logged (sensitive data check)
    - test_file_store_logs_path_and_size
    - test_auth_failure_logged_as_warn
    - test_chunk_integrity_failure_logged_as_error
```

## What This Is NOT

- NOT a log aggregation system (use Loki, ELK, or similar externally)
- NOT a log rotation system (use logrotate or systemd journal)
- NOT an alerting system (use AlertManager with Prometheus metrics)
- The logging system EMITS structured data. External tools CONSUME it.
