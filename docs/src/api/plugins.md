# Plugin Endpoints

AeorDB supports deploying WebAssembly (WASM) plugins that extend the database with custom logic. Plugins are scoped to a `{database}/{schema}/{table}` namespace.

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
curl -X PUT "http://localhost:3000/mydb/public/users/_deploy?name=my-plugin" \
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
curl -X POST http://localhost:3000/mydb/public/users/compute/_invoke \
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
curl http://localhost:3000/mydb/_plugins \
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
curl -X DELETE http://localhost:3000/mydb/public/users/compute/_remove \
  -H "Authorization: Bearer $TOKEN"
```

### Error Responses

| Status | Condition |
|--------|-----------|
| 404 | Plugin not found |
| 500 | Removal failure |
