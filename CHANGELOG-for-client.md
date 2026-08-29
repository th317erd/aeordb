# Server Changes Since 2026-04-24

Changelog of AeorDB server changes relevant to the client team.

---

## V4/NVT/GC Campaign Status (2026-08-29)

- The server now contains the independently tested v4 format, semantic-root,
  lifecycle/GC, native-index, root-aware query, and side-by-side migration
  substrate.
- Ordinary service startup still selects the v3 compatibility runtime. There is
  no public `migrate`/`cutover` CLI or HTTP route and no automatic v4 format
  activation. Client applications must not infer v4 acceptance from a binary
  upgrade, healthy restart, inactive/installed index-runtime metrics, reindex,
  import, or root promotion.
- Query/search/list/fetch support exact selected-root behavior. Query and search
  return the selected `root` metadata; related fetches should reuse that exact
  `root_hash`. APOS pagination tokens are canonical unpadded base64url and must
  remain bound to their explicit root.
- Legacy-v3 non-JSON historical search can return
  `503 HISTORICAL_VIEW_UNAVAILABLE` when exact parser semantics cannot be
  reconstructed. Clients must surface that error rather than retry against a
  different root.
- Root-only operational surfaces expose bounded durability, repair, memory,
  configuration, GC, and v4 index-runtime status. An inactive v4 runtime is
  expected on the current v3 service.
- Copied-production rehearsal, canary, installation/deployment, cutover,
  operator acceptance, first v4 write, monitoring, and destructive v4 GC are
  separate release gates. No client rollout should assume a later gate was
  approved because an earlier one passed.

See the served **V3-to-V4 Migration and Cutover** operations page for the
current boundary and rollback model.

---

## Breaking Changes

### `ApiKeyRecord.user_id` is now `Option<Uuid>`
- Previously `Uuid`, now `Option<Uuid>` (nullable)
- Keys with `user_id: null` are **share keys** — their rules are the sole permission authority
- Token exchange for share keys produces JWT with `sub: "share:{key_id}"`
- Client code that assumes `user_id` is always present needs updating

### Inline upload limit removed
- The 100MB inline upload limit no longer exists
- Uploads now stream in 256KB chunks server-side — the full file is never buffered
- Files up to 10GB are supported via standard `PUT /files/{path}`
- No client-side changes needed — the same PUT endpoint works

### `PATCH /files/{path}` now requires Update permission
- Previously PATCH (rename/move) was mapped to Read operation, allowing renames with read-only tokens
- Now correctly mapped to Update (`u` flag in crudlify)

---

## New Endpoints

### `POST /files/share-link`
Create a shareable URL backed by a scoped API key.

```json
POST /files/share-link
{
  "paths": ["/photos/vacation/"],
  "permissions": "-r--l---",
  "expires_in_days": 30,
  "base_url": "http://example.com:6830"
}
```

Returns `{ url, token, key_id, permissions, expires_at, paths }`.

### `GET /files/share-links?path=...`
List active share links for a path. Root only.

### `DELETE /files/share-links/{key_id}`
Revoke a share link. Root only.

### `GET /files/shared-with-me`
Returns all paths where the calling user has `.permissions` access. Used for discovering shared content when the user has no root-level permissions.

```json
{ "paths": [{ "path": "/photos/vacation/", "permissions": ".r..l...", "path_pattern": null }] }
```

### `GET /system/email-config`
Get email configuration (secrets masked). Root only.

### `PUT /system/email-config`
Save email configuration (SMTP or OAuth). Root only.

### `POST /system/email-test`
Send a test email. Root only.

---

## New Behaviors

### Directory listings include `effective_permissions`
When accessed with a scoped API key OR as a non-root user, each item in directory listings now includes an `effective_permissions` field (8-char crudlify string). Use this to determine what UI actions to show/hide.

```json
{
  "items": [
    {
      "name": "sunset.jpg",
      "path": "/photos/sunset.jpg",
      "effective_permissions": ".r..l...",
      ...
    }
  ]
}
```

### Ancestor-aware path matching for scoped keys
Scoped API keys (including share keys) can now navigate ancestor directories. If a key is scoped to `/photos/vacation/**`, the user can list `/`, `/photos/`, etc. — but only sees the path components leading to the scoped target. No sibling directories are exposed.

### `?token=` query parameter authentication
The auth middleware now accepts `?token=JWT` as an alternative to `Authorization: Bearer`. Used for share links and media streaming. Responses include `Cache-Control: no-store` and `Referrer-Policy: no-referrer` headers.

### Empty directory deletion
`DELETE /files/{path}` now works for empty directories. Non-empty directories return `400 Bad Request` with the child count.

### Email notifications on share
When `POST /files/share` is called, background emails are sent to recipients (if email is configured via `PUT /system/email-config`).

### Email required on user creation
`POST /system/users` now requires the `email` field (previously optional).

---

## Security Changes

- Share tokens (`user_id: null` keys) are blocked from: `GET /system/stats`, `GET /files/shares`, `POST /files/share`, `POST /files/share-link`
- `GET /auth/keys/users` now filters out inactive (deactivated) users
- Share routes block `/.system/` paths
- Empty API key rules on share keys return 403 (prevent full-access bypass)
- SSRF protection on custom OAuth URLs (HTTPS required, private IPs blocked)
- Email addresses validated for CRLF injection

---

## Web Components Changes

If the client uses shared web components from `aeordb-web-components`:

- **Grid view**: Image thumbnails loaded with auth, SVG file type icons (folder, video, audio, PDF, code, archive, text)
- **Grid view**: Video thumbnails via frame capture (range requests, seeks past intros)
- **Video/audio preview**: Uses `?token=` URL for streaming (no full download)
- **Share modal**: Proper tabs (People/Link), `<aeor-crudlify>` component for custom permissions
- **Permission-aware UI**: Buttons (Delete, Upload, New Folder, Rename, Share) toggle based on `effective_permissions`
- **`flashButton()` utility**: Exported from `utils.js` for success/error button feedback
- **`_openTab()` accepts `initialPath`**: For share sessions that start at a specific directory
- **`fileExtension`, `isVideoFile`, `isAudioFile`, `flashButton`** now exported from `aeor-file-view-shared.js`
