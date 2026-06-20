# Bot Quickstart

This page is for automated agents that discover an AeorDB HTTP endpoint and need to understand how to use it safely.

## Discovery

- The public health endpoint is `GET /system/health`.
- The public documentation is served at `GET /docs/`.
- This raw bot guide is served at `GET /docs/SKILL.md`.
- The browser portal is served at `GET /`.

## Authentication

Most API routes require an API key exchanged for a bearer token:

```bash
curl -X POST "$AEORDB/auth/token" \
  -H "Content-Type: application/json" \
  -d '{"api_key":"<api-key>"}'
```

Use the returned token as:

```bash
Authorization: Bearer <token>
```

If auth is disabled for a development instance, authenticated routes may work without this header.

## High-Value Routes

| Purpose | Route |
|---------|-------|
| Health | `GET /system/health` |
| Stats | `GET /system/stats` |
| Read file | `GET /files/{path}` |
| Write file | `PUT /files/{path}` |
| List directory | `GET /files/{dir}/` |
| Query one subtree | `POST /files/query` |
| Search globally or by subtree | `POST /files/search` |
| Fetch many files or ranges | `POST /files/fetch` |
| Chunk upload check | `POST /blobs/check` |
| Upload chunk | `PUT /blobs/chunks/{hash}` |
| Commit uploaded chunks | `POST /blobs/commit` |
| Invoke plugin | `POST /plugins/{name}/invoke` |
| Events | `GET /system/events` |

## Search Examples

Search one folder:

```json
{
  "path": "/docs/",
  "query": "how to",
  "limit": 20
}
```

Structured search:

```json
{
  "path": "/",
  "where": {"field": "@extension", "op": "eq", "value": "md"},
  "limit": 100
}
```

Search with locators for follow-up range fetch:

```json
{
  "path": "/docs/",
  "query": "database write pattern",
  "include_matches": true,
  "max_matches_per_result": 5,
  "snippet_chars": 240
}
```

Use returned `fetch_hint`, `ranges`, `content_hash`, and `updated_at` with `POST /files/fetch` to retrieve only the relevant parts of large files.

## Range Fetch

Use `POST /files/fetch` for batch reads and partial reads. Prefer range fetch after search hit locators instead of downloading large documents.

## Safety Rules

- Treat AeorDB as a database, not a disposable file server.
- Do not run repair, GC, import, export-over, or repeated restarts against a suspected corrupt original until evidence has been preserved.
- If corruption is suspected, preserve the database file, hot files, lock file, and logs before mutation.
- Prefer graceful shutdown with SIGTERM/Ctrl+C and wait for completion.
- Avoid broad unbounded searches or full-file fetches when a scoped search or range fetch will do.

## More Detail

Start with:

- `GET /docs/api/files.html`
- `GET /docs/api/querying.html`
- `GET /docs/api/upload-protocol.html`
- `GET /docs/api/plugins.html`
- `GET /docs/operations/threat-model.html`
