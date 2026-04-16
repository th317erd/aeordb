# Authentication

AeorDB supports multiple authentication modes. All protected endpoints require either a JWT Bearer token or are accessed through an API key exchange.

## Auth Modes

AeorDB can run in one of three authentication modes, selected at startup:

| Mode | CLI Flag | Description |
|------|----------|-------------|
| `disabled` | `--auth disabled` | No authentication. All requests are allowed. |
| `self-contained` | `--auth self-contained` | Keys and users stored inside the database (default). |
| `file` | `--auth file://<path>` | Identity loaded from an external file. Returns a bootstrap API key on first run. |

### Disabled Mode

All middleware is bypassed. Every request is treated as authenticated. Useful for local development.

### Self-Contained Mode

The default. Users, API keys, and tokens are all stored within the AeorDB engine itself. The JWT signing key is generated automatically.

### File Mode

Identity is loaded from an external file at the specified path. On first startup, a bootstrap API key is printed to stdout so you can authenticate and set up additional users.

---

## Endpoint Summary

| Method | Path | Description | Auth Required |
|--------|------|-------------|---------------|
| POST | `/auth/token` | Exchange API key for JWT | No |
| POST | `/auth/magic-link` | Request a magic link | No |
| GET | `/auth/magic-link/verify` | Verify a magic link code | No |
| POST | `/auth/refresh` | Refresh an expired JWT | No |
| POST | `/admin/api-keys` | Create an API key | Yes (root) |
| GET | `/admin/api-keys` | List API keys | Yes (root) |
| DELETE | `/admin/api-keys/{key_id}` | Revoke an API key | Yes (root) |
| POST | `/api-keys` | Create an API key (self-service) | Yes |
| GET | `/api-keys` | List your own API keys | Yes |
| DELETE | `/api-keys/{key_id}` | Revoke your own API key | Yes |

---

## JWT Tokens

All protected endpoints accept a JWT Bearer token in the `Authorization` header:

```
Authorization: Bearer eyJhbGciOiJIUzI1NiIs...
```

### Token Claims

| Claim | Type | Description |
|-------|------|-------------|
| `sub` | string | User ID (UUID) or email |
| `iss` | string | Always `"aeordb"` |
| `iat` | integer | Issued-at timestamp (Unix seconds) |
| `exp` | integer | Expiration timestamp (Unix seconds) |
| `scope` | string | Optional scope restriction |
| `permissions` | object | Optional fine-grained permissions |

---

## POST /auth/token

Exchange an API key for a JWT and refresh token. This is the primary authentication flow.

### Request Body

```json
{
  "api_key": "aeor_660e8400_a1b2c3d4e5f6..."
}
```

### Response

**Status:** `200 OK`

```json
{
  "token": "eyJhbGciOiJIUzI1NiIs...",
  "expires_in": 3600,
  "refresh_token": "rt_a1b2c3d4e5f6..."
}
```

| Field | Type | Description |
|-------|------|-------------|
| `token` | string | JWT access token |
| `expires_in` | integer | Token lifetime in seconds |
| `refresh_token` | string | Refresh token for obtaining new JWTs |

### API Key Format

API keys follow the format `aeor_{key_id_prefix}_{secret}`. The `key_id_prefix` is extracted for O(1) lookup -- the server does not iterate all stored keys.

### Example

```bash
curl -X POST http://localhost:3000/auth/token \
  -H "Content-Type: application/json" \
  -d '{"api_key": "aeor_660e8400_a1b2c3d4e5f6..."}'
```

### Error Responses

| Status | Condition |
|--------|-----------|
| 401 | Invalid, revoked, or malformed API key |
| 500 | Token creation failure |

---

## POST /auth/magic-link

Request a magic link for passwordless authentication. The server always returns `200 OK` regardless of whether the email exists, to prevent email enumeration.

In development mode, the magic link URL is logged via tracing (no email is actually sent).

### Rate Limiting

This endpoint is rate-limited per email address. Exceeding the limit returns `429 Too Many Requests`.

### Request Body

```json
{
  "email": "alice@example.com"
}
```

### Response

**Status:** `200 OK`

```json
{
  "message": "If an account exists, a login link has been sent."
}
```

### Example

```bash
curl -X POST http://localhost:3000/auth/magic-link \
  -H "Content-Type: application/json" \
  -d '{"email": "alice@example.com"}'
```

### Error Responses

| Status | Condition |
|--------|-----------|
| 429 | Rate limit exceeded |

---

## GET /auth/magic-link/verify

Verify a magic link code and receive a JWT. Each code can only be used once.

### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `code` | string | Yes | The magic link code |

### Response

**Status:** `200 OK`

```json
{
  "token": "eyJhbGciOiJIUzI1NiIs...",
  "expires_in": 3600
}
```

### Example

```bash
curl "http://localhost:3000/auth/magic-link/verify?code=abc123..."
```

### Error Responses

| Status | Condition |
|--------|-----------|
| 401 | Invalid code, expired, or already used |
| 500 | Token creation failure |

---

## POST /auth/refresh

Exchange a refresh token for a new JWT and a new refresh token. Implements **token rotation** -- the old refresh token is revoked and cannot be reused.

### Request Body

```json
{
  "refresh_token": "rt_a1b2c3d4e5f6..."
}
```

### Response

**Status:** `200 OK`

```json
{
  "token": "eyJhbGciOiJIUzI1NiIs...",
  "expires_in": 3600,
  "refresh_token": "rt_new_token_here..."
}
```

### Example

```bash
curl -X POST http://localhost:3000/auth/refresh \
  -H "Content-Type: application/json" \
  -d '{"refresh_token": "rt_a1b2c3d4e5f6..."}'
```

### Error Responses

| Status | Condition |
|--------|-----------|
| 401 | Invalid, revoked, or expired refresh token |
| 500 | Token creation failure |

---

## Root User

The root user has the nil UUID (`00000000-0000-0000-0000-000000000000`). Only the root user can:

- Create and manage API keys
- Create and manage users
- Create and manage groups
- Restore snapshots and manage forks
- Run garbage collection
- Manage tasks and cron schedules
- Export, import, and promote versions

### First-Run Bootstrap

When using `file://` auth mode, a bootstrap API key is printed to stdout on first run. Use this key to authenticate as root and create additional users and keys:

```bash
# Start the server (prints bootstrap key)
aeordb --auth file:///path/to/identity.json

# Exchange the bootstrap key for a token
curl -X POST http://localhost:3000/auth/token \
  -H "Content-Type: application/json" \
  -d '{"api_key": "<bootstrap-key>"}'
```

---

## Authentication Flow Summary

```
                 API Key                    Magic Link
                   |                            |
          POST /auth/token             POST /auth/magic-link
                   |                            |
                   v                            v
              JWT + Refresh               Email with code
                   |                            |
                   |                   GET /auth/magic-link/verify
                   |                            |
                   v                            v
              Use JWT in                    JWT Token
           Authorization header                 |
                   |                            |
                   v                            v
           Protected endpoints          Protected endpoints
                   |
          Token expires
                   |
         POST /auth/refresh
                   |
                   v
           New JWT + New Refresh
           (old refresh revoked)
```

---

## API Keys (Admin)

The `/admin/api-keys` endpoints listed in the endpoint summary are for root administrators managing any user's keys.

> **Note:** The `/admin/api-keys` endpoints are for root administrators managing any user's keys. For self-service key management, see [Self-Service API Keys](#self-service-api-keys) below.

---

## Self-Service API Keys

Any authenticated user can create, list, and revoke their own API keys. Root users can additionally create keys for other users.

### POST /api-keys

Create an API key for yourself.

**Request Body:**

```json
{
  "label": "MacBook sync client",
  "expires_in_days": 730,
  "rules": [
    {"/assets/**": "-r--l---"},
    {"/drafts/**": "crudlify"},
    {"**": "--------"}
  ]
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `label` | string | No | Human-friendly name for the key |
| `expires_in_days` | integer | No | Days until expiry (default: 730, max: 3650) |
| `rules` | array | No | Path permission rules (default: empty = full pass-through) |
| `user_id` | string | No | Root only: create key for another user |

**Response:** `201 Created`

```json
{
  "key_id": "a1b2c3d4-...",
  "key": "aeor_k_a1b2c3d4..._...",
  "user_id": "e5f6a7b8-...",
  "label": "MacBook sync client",
  "expires_at": 1839024000000,
  "rules": [...]
}
```

The `key` field (plaintext) is returned **once** and can never be retrieved again. Store it securely.

**Example:**

```bash
curl -X POST http://localhost:3000/api-keys \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "label": "CI deploy key",
    "expires_in_days": 90,
    "rules": [
      {"/deployments/**": "crudlify"},
      {"**": "--------"}
    ]
  }'
```

### GET /api-keys

List your own API keys (non-revoked). Root users see all keys.

**Response:** `200 OK`

```json
[
  {
    "key_id": "a1b2c3d4-...",
    "label": "MacBook sync client",
    "user_id": "e5f6a7b8-...",
    "expires_at": 1839024000000,
    "created_at": 1776208000000,
    "rules": [...]
  }
]
```

Never includes the key hash or plaintext.

### DELETE /api-keys/{key_id}

Revoke one of your own API keys. Root users can revoke anyone's key.

**Response:** `200 OK`

```json
{
  "revoked": true,
  "key_id": "a1b2c3d4-..."
}
```

---

## Scoped API Keys

API keys can be restricted to specific paths and operations using **rules**. Rules are an ordered list of path-glob to permission-flags pairs. The first matching rule wins.

### Rule Format

Each rule is a JSON object with one key (the glob pattern) and one value (the permission flags):

```json
[
  {"/assets/**": "-r--l---"},
  {"/drafts/**": "crudlify"},
  {"**": "--------"}
]
```

### Permission Flags

The flags string is exactly 8 characters, one for each operation:

| Position | Flag | Operation |
|----------|------|-----------|
| 0 | `c` | Create |
| 1 | `r` | Read |
| 2 | `u` | Update |
| 3 | `d` | Delete |
| 4 | `l` | List |
| 5 | `i` | Invoke |
| 6 | `f` | Functions (deploy) |
| 7 | `y` | Configure |

Use the letter to allow the operation, `-` to deny it:

- `crudlify` — full access
- `-r--l---` — read and list only
- `cr------` — create and read only
- `--------` — deny all

### Rule Evaluation

1. Rules are evaluated top-to-bottom. The **first matching glob** determines the permissions.
2. If no rule matches the path, access is **denied**.
3. An empty rules list means no restrictions (full pass-through to user permissions).
4. Rules can only **restrict** — they never grant more access than the user already has.

### Security Behavior

When a scoped key is denied access to a path, the server returns **404 Not Found** (not 403 Forbidden). This prevents information leakage — a denied path looks identical to a path that doesn't exist.

This also applies to directory listings: entries the key cannot access are silently omitted from the response.

### Key Expiration

All API keys have a mandatory expiration:

- **Default:** 730 days (2 years)
- **Maximum:** 3650 days (10 years)
- Expired keys are rejected at token exchange time

### Examples

**Read-only key for `/assets/`:**

```json
{
  "label": "Asset viewer",
  "rules": [
    {"/assets/**": "-r--l---"},
    {"**": "--------"}
  ]
}
```

**Full access to one project, read-only elsewhere:**

```json
{
  "label": "Project lead - Q4 campaign",
  "rules": [
    {"/projects/q4-campaign/**": "crudlify"},
    {"/shared/**": "-r--l---"},
    {"**": "--------"}
  ]
}
```

**CI/CD deploy key (create and update only, specific path):**

```json
{
  "label": "CI pipeline",
  "expires_in_days": 90,
  "rules": [
    {"/deployments/**": "cru-----"},
    {"**": "--------"}
  ]
}
```
