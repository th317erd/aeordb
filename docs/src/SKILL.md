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
| Root operator status | `aeordb status --target <url> --json` |
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

`GET /system/stats` is available to authenticated users, but filesystem paths in administrative configuration/recovery fields are redacted for non-root callers. Root responses include one current/latest GC status at `health.gc` after a run begins; non-root responses omit it. The Prometheus endpoint and `metrics`/`gc_status` SSE events are root-only. Bots with root credentials should prefer `aeordb status --json` for one bounded operational snapshot and subscribe to `gc_status` only when immediate GC transitions matter.

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
  "root_hash": "9f26...",
  "query": "database write pattern",
  "include_matches": true,
  "max_matches_per_result": 5,
  "snippet_chars": 240
}
```

Successful query/search responses include the exact selected root as
`root.hash`. Locator-bearing results include `file_key`, `record_revision`,
`content_hash`, `matches[].range`, and `matches[].fetch`. Use those identities
with `POST /files/fetch` to retrieve only the relevant parts of large files.
Legacy-v3 non-JSON searches can return `503 HISTORICAL_VIEW_UNAVAILABLE` when
their parser semantics depend on the detached root-level parser registry;
retrying against a different root must not be used to disguise that exactness
failure.

For an exact byte-range locator, the follow-up shape is:

```json
{
  "root_hash": "<query-or-search root.hash>",
  "items": [{
    "path": "<result path>",
    "if_content_hash": "<result content_hash>",
    "range": {
      "mode": "bytes",
      "start": 6,
      "end": 12
    }
  }]
}
```

Do not omit `root_hash`: doing so reads current HEAD and can return different
bytes from the same path after a mutation. Query/search APOS cursors are
canonical unpadded base64url tokens; `after` or `before` must be sent with the
exact explicit `root_hash` returned by the preceding response.

## Range Fetch

Use `POST /files/fetch` for batch reads and partial reads. Prefer range fetch after search hit locators instead of downloading large documents.

When an exact root is known, put one of `root_hash`, `snapshot`, or `version`
in the POST JSON body; never combine them. Successful whole/range fetch and ZIP
responses preserve their legacy bodies and return the exact selected root in
`X-AeorDB-Root-Hash`, `X-AeorDB-Root-State`, and
`X-AeorDB-Root-Expires-At`. Reuse the same `root_hash` for related reads instead
of silently accepting current HEAD. Share-link credentials are current-only.

## Safety Rules

- Treat AeorDB as a database, not a disposable file server.
- Do not run repair, GC, import, export-over, or repeated restarts against a suspected corrupt original until evidence has been preserved.
- If corruption is suspected, preserve the database file, lock file, configured external spill evidence, and logs before mutation. Current hot-tail recovery state is inside the database file.
- Prefer graceful shutdown with SIGTERM/Ctrl+C and wait for completion.
- Avoid broad unbounded searches or full-file fetches when a scoped search or range fetch will do.
- Ordinary service operation still uses the v3 compatibility runtime.
  `aeordb migrate-v4` may build and verify a separate shadow from an authorized
  offline v3 copy, but there is no public cutover command/route and no automatic
  v4 activation. Never invoke internal migration modules, rename database
  artifacts by hand, or infer v4 acceptance from a shadow receipt or healthy
  restart.
- Copied-production rehearsal, canary, installation/deployment, cutover,
  operator acceptance, first v4 write, and destructive v4 GC are distinct
  authorization boundaries. Approval for one does not approve the next.

## More Detail

Start with:

- `GET /docs/api/files.html`
- `GET /docs/api/querying.html`
- `GET /docs/api/upload-protocol.html`
- `GET /docs/api/plugins.html`
- `GET /docs/operations/migration.html`
- `GET /docs/operations/threat-model.html`
