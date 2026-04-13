# CORS Configuration

AeorDB supports Cross-Origin Resource Sharing (CORS) through a CLI flag and per-path rules stored in the database.

## Configuration Methods

### CLI Flag

Enable CORS at startup with the `--cors` flag:

```bash
# Allow all origins
aeordb --cors "*"

# Allow specific origins
aeordb --cors "https://app.example.com,https://admin.example.com"
```

The CLI flag sets the **default** CORS policy for all routes. When no `--cors` flag is provided, no CORS headers are sent.

### Per-Path Rules (Config File)

For fine-grained control, store per-path CORS rules at `/.config/cors.json` inside the database:

```json
{
  "rules": [
    {
      "path": "/engine/*",
      "origins": ["https://app.example.com"],
      "methods": ["GET", "POST", "PUT", "DELETE", "HEAD", "OPTIONS"],
      "allow_headers": ["Content-Type", "Authorization"],
      "max_age": 3600,
      "allow_credentials": false
    },
    {
      "path": "/query",
      "origins": ["*"],
      "methods": ["POST"],
      "allow_headers": ["Content-Type", "Authorization"],
      "max_age": 600,
      "allow_credentials": false
    },
    {
      "path": "/events/stream",
      "origins": ["https://app.example.com"],
      "allow_credentials": true
    }
  ]
}
```

Upload the config file using the engine API:

```bash
curl -X PUT http://localhost:3000/engine/.config/cors.json \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d @cors.json
```

---

## Rule Schema

Each rule in the `rules` array supports:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `path` | string | (required) | URL path to match. Supports trailing `*` for prefix matching. |
| `origins` | array of strings | (required) | Allowed origins. Use `["*"]` for any origin. |
| `methods` | array of strings | `["GET","POST","PUT","DELETE","HEAD","OPTIONS"]` | Allowed HTTP methods. |
| `allow_headers` | array of strings | `["Content-Type","Authorization"]` | Allowed request headers. |
| `max_age` | integer | `3600` | Preflight cache duration in seconds. |
| `allow_credentials` | boolean | `false` | Whether to include `Access-Control-Allow-Credentials: true`. |

---

## Path Matching

Per-path rules are checked in order (first match wins):

- **Exact match:** `"/query"` matches only `/query`
- **Prefix match:** `"/engine/*"` matches `/engine/data/file.json`, `/engine/images/photo.png`, etc.

If no per-path rule matches, the CLI default (if any) is used.

---

## Precedence

1. Per-path rules from `/.config/cors.json` (first match wins)
2. CLI `--cors` flag defaults
3. No CORS headers (if neither is configured)

---

## CORS Middleware Behavior

The CORS middleware runs as the **outermost layer** in the middleware stack, ensuring that OPTIONS preflight requests are handled before authentication middleware rejects them for missing tokens.

### Preflight Requests (OPTIONS)

When a preflight request arrives:

1. The middleware checks if the `Origin` header matches an allowed origin.
2. If **allowed**: returns `204 No Content` with CORS headers:
   - `Access-Control-Allow-Origin`
   - `Access-Control-Allow-Methods`
   - `Access-Control-Allow-Headers`
   - `Access-Control-Max-Age`
   - `Access-Control-Allow-Credentials` (if configured)
3. If **not allowed**: returns `403 Forbidden`.

### Normal Requests

For non-preflight requests with an allowed origin:

1. The request passes through to the handler normally.
2. CORS headers are appended to the response:
   - `Access-Control-Allow-Origin`
   - `Access-Control-Allow-Credentials` (if configured)

For requests from non-allowed origins, no CORS headers are added (the browser will block the response).

### Wildcard Origin

When origins include `"*"`, the `Access-Control-Allow-Origin` header is set to `*`. Note: when using `allow_credentials: true`, browsers require a specific origin rather than `*`.

---

## Default CORS Headers (CLI Flag)

When only the `--cors` flag is used (no per-path rules), the defaults are:

| Header | Value |
|--------|-------|
| `Access-Control-Allow-Methods` | `GET, POST, PUT, DELETE, HEAD, OPTIONS` |
| `Access-Control-Allow-Headers` | `Content-Type, Authorization` |
| `Access-Control-Max-Age` | `3600` |
| `Access-Control-Allow-Credentials` | Not set |

---

## Examples

### Development: Allow Everything

```bash
aeordb --cors "*"
```

### Production: Specific Origins

```bash
aeordb --cors "https://app.example.com,https://admin.example.com"
```

### Per-Path with Credentials

Store in `/.config/cors.json`:

```json
{
  "rules": [
    {
      "path": "/events/stream",
      "origins": ["https://app.example.com"],
      "allow_credentials": true,
      "max_age": 86400
    },
    {
      "path": "/engine/*",
      "origins": ["https://app.example.com", "https://admin.example.com"],
      "methods": ["GET", "PUT", "DELETE", "HEAD", "OPTIONS"],
      "allow_headers": ["Content-Type", "Authorization", "X-Request-ID"]
    }
  ]
}
```
