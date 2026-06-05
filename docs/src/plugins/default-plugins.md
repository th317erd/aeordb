# Default Plugins

AeorDB ships first-party WASM query plugins under `aeordb-plugins/`. Release WASM builds for these plugins are embedded into the AeorDB server binary and installed at startup into user-accessible plugin paths.

On startup, AeorDB installs these bundled plugins if they are missing or if the stored WASM checksum differs from the embedded copy:

| Plugin | Public invoke path |
|--------|--------------------|
| `extract` | `POST /plugins/extract/invoke` |
| `jq` | `POST /plugins/jq/invoke` |

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

User-deployed plugins still use the normal plugin deployment API. The bundled plugin paths are restored to the embedded versions on startup when their checksums differ.

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
