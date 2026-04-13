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
