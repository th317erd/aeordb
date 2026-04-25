# File Sharing — Phase 2 Design Spec

**Date:** 2026-04-25
**Status:** Approved
**Phase:** 2 of 3 (Link sharing via scoped API keys)

## Overview

Add a Link tab to the Share modal that creates shareable URLs backed by scoped API keys. A share link is a JWT token (generated from a user-less API key) embedded in a URL. The key's crudlify rules are the sole permission authority — no user/group resolution needed.

Revoking the API key kills the link. Share keys are visible in the Keys page with descriptive labels.

---

## How It Works

1. User opens Share modal → Link tab
2. Chooses permission level (View only, Can edit, Full access, Custom)
3. Optionally sets expiration (default: never)
4. Backend creates a scoped API key with `user_id: None`, rules locked to the shared path(s)
5. Backend exchanges the key for a JWT with matching expiration (or no expiration)
6. Returns a copyable URL with the JWT embedded

---

## Share Key Conventions

**API key record:**
- `user_id: None` — signals this is a share key, not a user key
- `label`: descriptive, e.g. `"Share: /photos/ (View only)"`
- `rules`: path-scoped crudlify rules, e.g. `[{"/photos/**": "cr..l..."}, {"**": "........"}]`
- `expires_at`: user-chosen or `None` (never expires)

**JWT token:**
- `sub`: `"share:{key_id}"` — clearly not a UUID, identifies the share key
- `exp`: matches key expiration (omitted if no expiration)
- `key_id`: the API key ID (for revocation checks)

---

## Permission Flow for Share Keys

Current flow (normal user request):
1. JWT → extract user_id (UUID)
2. API key rules → restrict user's existing permissions
3. Permission resolver → check user's groups against `.permissions` files

Share key flow (user_id is None):
1. JWT → extract `sub` starting with `"share:"`, extract key_id
2. API key rules → **are** the effective permissions (grant, not restrict)
3. Permission resolver → **skipped entirely**

**Detection logic:** When `user_id` is `None` on the API key record (or `sub` fails UUID parse), treat the key's rules as grants. No user/group permission resolution.

---

## URL Format

**Portal view (file browser scoped to shared path):**
```
http://host:port/system/portal/?token=JWT&path=/photos/
```

**Direct file access (streams raw file):**
```
http://host:port/files/photos/sunset.jpg?token=JWT
```

---

## Auth Middleware Change

Accept `?token=` query parameter as an alternative to `Authorization: Bearer` header:

1. Check `Authorization` header first (existing behavior)
2. If absent, check `?token=` query parameter
3. Same JWT validation, same key_id revocation check
4. If the key has `user_id: None`, set a flag on the request context so the permission middleware knows to use key rules as grants

---

## Portal Changes

**Token detection on load:**
- On page load, check URL for `?token=` param
- If present, store it (sessionStorage or in-memory) and use it for all `window.api()` calls
- Skip the login screen
- If `?path=` is also present, navigate the file browser to that path

**Link tab in Share modal:**

The Link tab (currently greyed out) becomes active:

- **Expiration selector**: "Never", "1 hour", "1 day", "7 days", "30 days", custom
- **Permission level**: same dropdown as People tab (View only, Can edit, Full access, Custom with crudlify toggles)
- **"Create Link" button**: calls `POST /files/share-link`, shows the URL
- **Copy button**: copies URL to clipboard
- **Active links section**: lists existing share links for this path with expiration, permission level, and revoke (×) button

---

## Backend Endpoints

### `POST /files/share-link`

Create a share link (scoped API key + JWT).

**Request:**
```json
{
  "paths": ["/photos/"],
  "permissions": "cr..l...",
  "expires_in_days": null,
  "base_url": "http://myserver.com:6830"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `paths` | array | Yes | Paths to share |
| `permissions` | string | Yes | 8-character crudlify flags |
| `expires_in_days` | integer | No | Days until expiry, null = never |
| `base_url` | string | No | Base URL for the share link (default: derived from request Host header) |

**What it does:**
1. Build rules array from paths: each path gets `"{path}**": "{permissions}"`, plus `"**": "........"` deny-all fallback
2. Create API key with `user_id: None`, generated label, rules, expiration
3. Generate JWT with `sub: "share:{key_id}"`, matching expiration
4. Build URL: `{base_url}/system/portal/?token={jwt}&path={first_path}`
5. Return response

**Response:**
```json
{
  "url": "http://host:port/system/portal/?token=eyJ...&path=/photos/",
  "token": "eyJ...",
  "key_id": "a1b2c3d4-...",
  "permissions": "cr..l...",
  "expires_at": null,
  "paths": ["/photos/"]
}
```

### `GET /files/share-links?path=...`

List active share links for a path. Filters API keys where `user_id` is None and rules match the queried path.

**Response:**
```json
{
  "path": "/photos/",
  "links": [
    {
      "key_id": "a1b2c3d4-...",
      "label": "Share: /photos/ (View only)",
      "permissions": "cr..l...",
      "expires_at": null,
      "created_at": 1777081966860
    }
  ]
}
```

### `DELETE /files/share-links/{key_id}`

Revoke a share link by deleting the API key. Also revokes the JWT (middleware checks key_id on every request).

**Response:**
```json
{
  "revoked": true,
  "key_id": "a1b2c3d4-..."
}
```

---

## Keys Page Integration

Share keys appear in the Keys list alongside regular keys:
- Label shows `"Share: /photos/ (View only)"`
- Revoking from the Keys page kills the share link
- No special UI treatment needed — they're just API keys with `user_id: None`

---

## API Key Model Change

The `user_id` field on API keys must become `Option<Uuid>` (nullable):
- `Some(uuid)` → normal user key (existing behavior)
- `None` → share key (rules are grants, no permission resolver)

Check all code that assumes `user_id` is always present and handle `None`.

---

## Testing Strategy

**Share link endpoints:**
- Create share link → key created with correct rules and no user_id
- Create with expiration → key and JWT have matching expiry
- Create with no expiration → key and JWT have no expiry
- List share links → returns only share keys matching the path
- Revoke share link → key deleted, JWT rejected on next request

**Auth middleware:**
- `?token=JWT` in query param → authenticated (same as Bearer header)
- Share key JWT → permission resolver skipped, key rules used as grants
- Revoked share key → 401
- Expired share key → 401

**Permission flow:**
- Share key with `cr..l...` on `/photos/**` → can read/list /photos/, cannot write
- Share key with `crudlify` on `/docs/**` → full access to /docs/, denied elsewhere
- Normal user key → existing behavior unchanged

**Portal:**
- `?token=JWT&path=/photos/` → skips login, opens file browser at /photos/
- Direct file URL with `?token=JWT` → streams file content
