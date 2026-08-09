# Default Plugins

AeorDB ships first-party WASM query plugins under `aeordb-plugins/`. Release WASM builds for these plugins are embedded into the AeorDB server binary and installed at startup into user-accessible plugin paths.

On startup, AeorDB installs these bundled plugins if they are missing. If a plugin already exists at a bundled path, AeorDB overwrites it only when both conditions are true:

- The stored plugin ID matches the bundled plugin ID.
- The bundled plugin version is greater than or equal to the stored plugin version.

This makes checksum drift a signal, not the authority. A stored plugin at the same path with a different plugin ID is treated as user-managed and left untouched. A stored plugin with the bundled ID but a newer version is also left untouched to avoid downgrades.

The replacement decision is made while namespace mutation authority is held.
An unchanged bundle is a true no-op and does not consume a durability sequence
or require compile-memory admission. When an install may be needed, AeorDB
validates the bundled WASM and then rechecks the policy under authority before
writing. Each bundled record is atomic; if any required record is malformed,
oversized, unreadable, or cannot be durably stored, server construction fails
instead of exposing a ready server with partial runtime authority. A later
startup can safely retry any earlier per-record installation.

Bundled records use the plugin release timestamp for both `created_at` and `updated_at`, rather than the local startup time. Independently initialized nodes therefore store the same JSON bytes and content identity for the same bundled release. A current bundled record with legacy node-local timestamps is canonicalized once at startup when the ID and version replacement rules above allow it; later startups do not rewrite it.

| Plugin | Plugin ID | Version | Author | Public invoke path |
|--------|-----------|---------|--------|--------------------|
| `extract` | `/org/aeordev/aeordb/plugins/extract` | `0.1.0` | `AeorDB` | `POST /plugins/extract/invoke` |
| `jq` | `/org/aeordev/aeordb/plugins/jq` | `0.1.0` | `AeorDB` | `POST /plugins/jq/invoke` |

If you change a default plugin's source, rebuild its WASM and refresh the embedded copy before rebuilding AeorDB:

```bash
cd aeordb-plugins/extract-plugin
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/aeordb_extract_plugin.wasm \
  ../../aeordb-lib/src/plugins/bundled/extract.wasm

cd ../jq-plugin
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/aeordb_jq_plugin.wasm \
  ../../aeordb-lib/src/plugins/bundled/jq.wasm
```

User-deployed plugins still use the normal plugin deployment API. If a bundled
path is occupied by a plugin without the matching bundled plugin ID, startup
leaves it untouched and logs a warning. Removing a bundled record before
installing a user replacement gives that replacement a new ID and prevents the
default installer from taking the path back.

## `extract`

The `extract` plugin reads only the requested text range through the native plugin host extraction call. It does not buffer the whole file across the plugin boundary.

Request fields:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `file` | string | yes | File path to extract from |
| `path` | string | alias | Alias for `file` |
| `mode` | string | yes | `lines` or `chars` |
| `start` | integer | no | 1-based line start for `lines`, 0-based char start for `chars` |
| `end` | integer | no | Inclusive line end for `lines`, exclusive char end for `chars` |
| `max_bytes` | integer | no | Maximum returned text bytes |

Example:

```http
POST /plugins/extract/invoke
Content-Type: application/json

{
  "file": "/docs/readme.md",
  "mode": "lines",
  "start": 10,
  "end": 20,
  "max_bytes": 65536
}
```

Response body:

```json
{
  "text": "selected text\n",
  "content_type": "text/markdown",
  "source_size": 12345,
  "mode": "lines",
  "start": 10,
  "end": 20,
  "truncated": false
}
```

## `jq`

The `jq` plugin reads a JSON file and evaluates a jq-compatible expression using the embedded `jaq` engine. JSON files are currently loaded in full before filtering.

Request fields:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `file` | string | yes | JSON file path |
| `path` | string | alias | Alias for `file` |
| `expr` | string | yes | jq expression |

Example:

```http
POST /plugins/jq/invoke
Content-Type: application/json

{
  "file": "/data/messages.json",
  "expr": ".messages[] | select(.role == \"user\") | .content"
}
```

Responses always use a plural `outputs` array, even when the expression emits one value:

```json
{
  "outputs": [
    "first user message",
    "second user message"
  ]
}
```
