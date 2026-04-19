# Plugin Endpoints

AeorDB supports deploying WebAssembly (WASM) plugins that extend the database with custom logic. Plugins are scoped to a `{database}/{schema}/{table}` namespace.

## Native Parsers

AeorDB includes 8 built-in format parsers that run automatically during indexing -- no WASM deployment needed. Native parsers are tried first; if the content type is not recognized, the engine falls through to any deployed WASM parser plugin.

### Supported Formats

| Parser | Content Types | Extensions | Extracted Fields |
|--------|--------------|------------|------------------|
| **Text** | `text/plain`, `text/markdown`, `text/css`, `text/csv`, `application/json`, `application/xml`, `application/yaml`, `application/javascript`, `text/x-*` | `.txt`, `.md`, `.rs`, `.js`, `.py`, `.ts`, `.c`, `.h`, `.cpp`, `.java`, `.go`, `.sh`, `.css`, `.json`, `.yaml`, `.yml`, `.toml`, `.xml`, `.sql` | `text`, `metadata` (line/word/char counts, BOM detection, code heuristics) |
| **HTML/XML** | `text/html`, `text/xml`, `application/xhtml+xml` | `.html`, `.htm`, `.xhtml` | `text` (script/style stripped), `metadata` (title, description, keywords, headings, link count) |
| **PDF** | `application/pdf` | `.pdf` | `metadata` (page count, version) |
| **Images** | `image/jpeg`, `image/png`, `image/gif`, `image/bmp`, `image/webp`, `image/tiff`, `image/svg+xml` | `.jpg`, `.jpeg`, `.png`, `.gif`, `.bmp`, `.webp`, `.tiff`, `.tif`, `.svg` | `metadata` (format, dimensions, color depth from magic bytes) |
| **Audio** | `audio/mpeg`, `audio/mp3`, `audio/wav`, `audio/x-wav`, `audio/ogg`, `audio/vorbis` | `.mp3`, `.wav`, `.ogg` | `metadata` (format, duration, sample rate, channels, bitrate, ID3 tags) |
| **Video** | `video/mp4`, `video/quicktime`, `video/x-msvideo`, `video/avi`, `video/webm`, `video/x-matroska`, `video/x-flv` | `.mp4`, `.mov`, `.avi`, `.webm`, `.mkv`, `.flv` | `metadata` (format, container, stream info) |
| **MS Office** | `application/vnd.openxmlformats-officedocument.wordprocessingml.document`, `application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`, `application/msword`, `application/vnd.ms-excel` | `.docx`, `.xlsx` | Extracted text and document metadata |
| **ODF** | `application/vnd.oasis.opendocument.text`, `application/vnd.oasis.opendocument.spreadsheet` | `.odt`, `.ods` | Extracted text and document metadata |

### How Native Parsers Work

1. When a file is stored and indexing runs, the engine checks the file's content type against the native parser registry.
2. If the content type is `application/octet-stream` or empty, the engine falls back to extension-based matching.
3. If a native parser handles the format, it returns structured JSON that feeds into the indexing pipeline.
4. If no native parser matches, the engine falls through to the WASM plugin system (checking `/.config/parsers.json` or the directory's `parser` field).

Native parsers have zero overhead compared to WASM -- they run as compiled Rust code in the same process. WASM parser plugins remain available for custom or proprietary formats not covered by the built-in parsers.

---

## Endpoint Summary

| Method | Path | Description | Auth |
|--------|------|-------------|------|
| PUT | `/{db}/{schema}/{table}/_deploy` | Deploy a WASM plugin | Yes |
| POST | `/{db}/{schema}/{table}/{function}/_invoke` | Invoke a plugin function | Yes |
| GET | `/{db}/_plugins` | List all deployed plugins | Yes |
| DELETE | `/{db}/{schema}/{table}/{function}/_remove` | Remove a deployed plugin | Yes |

---

## PUT /{db}/{schema}/{table}/_deploy

Deploy a WASM plugin to the given namespace. If a plugin already exists at this path, it is replaced.

### Request

- **URL parameters:**
  - `{db}` -- database name
  - `{schema}` -- schema name
  - `{table}` -- table name
- **Query parameters:**
  - `name` (optional) -- plugin name (defaults to the `{table}` segment)
  - `plugin_type` (optional) -- plugin type string (defaults to `"wasm"`)
- **Headers:**
  - `Authorization: Bearer <token>` (required)
- **Body:** raw WASM binary bytes

### Response

**Status:** `200 OK`

Returns the plugin metadata:

```json
{
  "name": "my-plugin",
  "path": "mydb/public/users",
  "plugin_type": "wasm",
  "deployed_at": "2026-04-13T10:00:00Z"
}
```

### Example

```bash
curl -X PUT "http://localhost:6830/mydb/public/users/_deploy?name=my-plugin" \
  -H "Authorization: Bearer $TOKEN" \
  --data-binary @plugin.wasm
```

### Error Responses

| Status | Condition |
|--------|-----------|
| 400 | Empty body or invalid plugin type |
| 400 | Invalid WASM module |
| 500 | Deployment failure |

---

## POST /{db}/{schema}/{table}/{function}/_invoke

Invoke a deployed plugin's function. The request body is wrapped in a `PluginRequest` envelope with metadata, then passed to the WASM runtime.

### Request

- **URL parameters:**
  - `{db}` -- database name
  - `{schema}` -- schema name
  - `{table}` -- table name
  - `{function}` -- function name to invoke
- **Headers:**
  - `Authorization: Bearer <token>` (required)
  - `Content-Type` -- depends on what the plugin expects
- **Body:** raw request payload (passed to the plugin as `arguments`)

### Plugin Request Envelope

The server wraps the raw body into:

```json
{
  "arguments": "<raw body bytes>",
  "metadata": {
    "function_name": "compute",
    "path": "/mydb/public/users/compute",
    "plugin_path": "mydb/public/users"
  }
}
```

### Plugin Response

Plugins return a `PluginResponse` envelope:

```json
{
  "status_code": 200,
  "content_type": "application/json",
  "headers": {
    "x-custom-header": "value"
  },
  "body": "<response bytes>"
}
```

The server maps these fields to the HTTP response. For security, only safe headers are forwarded:
- Headers starting with `x-`
- `cache-control`, `etag`, `last-modified`, `content-disposition`, `content-language`, `content-encoding`, `vary`

If the plugin returns data that is not a valid `PluginResponse`, it is sent as raw `application/octet-stream` bytes (backward compatibility).

### WASM Host Functions

Plugins have access to the following host functions for interacting with the database:

- **CRUD:** read, write, and delete files
- **Query:** execute queries and aggregations against the engine
- **Context:** access request metadata

### Example

```bash
curl -X POST http://localhost:6830/mydb/public/users/compute/_invoke \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"input": "hello"}'
```

### Error Responses

| Status | Condition |
|--------|-----------|
| 404 | Plugin not found at the given path |
| 500 | Plugin invocation failure (runtime error, panic, etc.) |

---

## GET /{db}/_plugins

List all deployed plugins.

### Request

- **URL parameters:**
  - `{db}` -- database name
- **Headers:**
  - `Authorization: Bearer <token>` (required)

### Response

**Status:** `200 OK`

```json
[
  {
    "name": "my-plugin",
    "path": "mydb/public/users",
    "plugin_type": "wasm"
  }
]
```

### Example

```bash
curl http://localhost:6830/mydb/_plugins \
  -H "Authorization: Bearer $TOKEN"
```

---

## DELETE /{db}/{schema}/{table}/{function}/_remove

Remove a deployed plugin.

### Request

- **URL parameters:**
  - `{db}` -- database name
  - `{schema}` -- schema name
  - `{table}` -- table name
  - `{function}` -- function name
- **Headers:**
  - `Authorization: Bearer <token>` (required)

### Response

**Status:** `200 OK`

```json
{
  "removed": true,
  "path": "mydb/public/users"
}
```

### Example

```bash
curl -X DELETE http://localhost:6830/mydb/public/users/compute/_remove \
  -H "Authorization: Bearer $TOKEN"
```

### Error Responses

| Status | Condition |
|--------|-----------|
| 404 | Plugin not found |
| 500 | Removal failure |
