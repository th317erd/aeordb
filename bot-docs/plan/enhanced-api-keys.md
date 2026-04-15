# Enhanced API Key System — Design Spec

**Date:** 2026-04-14

---

## Overview

Overhaul the API key system to support self-service key management, scoped permissions via path-glob rules, mandatory expiration, and strict security posture. The primary driver is the `aeordb-client`, which needs users to create and manage their own API keys with appropriate restrictions.

## Design Principles

- **Principle of least privilege** — keys can only restrict, never expand beyond the user's own permissions
- **Denied = doesn't exist** — any access denied by key restrictions returns 404, never 403. No information leakage about resource existence.
- **Strict validation** — any missing, malformed, or corrupt auth field is a hard reject. No fallbacks, no defaults at validation time, no "best effort."
- **JWT verified first** — no database hit until the JWT signature is proven authentic in memory

---

## API Key Record

```rust
pub struct ApiKeyRecord {
    pub key_id: Uuid,
    pub key_hash: String,           // Argon2id hash (unchanged)
    pub user_id: Uuid,              // owner
    pub created_at: DateTime<Utc>,
    pub expires_at: i64,            // mandatory, ms since epoch
    pub is_revoked: bool,
    pub label: Option<String>,      // human-friendly name ("MacBook sync", "CI deploy")
    pub rules: Vec<KeyRule>,        // ordered path→permission rules
}

pub struct KeyRule {
    pub glob: String,       // path glob pattern (e.g. "/assets/**", "**")
    pub permitted: String,  // crudlify flags (e.g. "-r--l---", "crudlify")
}
```

### Expiration

- **Mandatory** on every key
- **Default:** 2 years (730 days) from creation when not specified
- **Maximum:** 10 years (3650 days) from creation
- Server enforces the ceiling — requests for longer are clamped to max

### Rules

Ordered list of `(glob, permitted_flags)` pairs. First matching glob wins.

```json
[
  {"/assets/**": "-r--l---"},
  {"/drafts/**": "crudlify"},
  {"**": "--------"}
]
```

- Each rule is a JSON object with one key (the glob) and one value (the flags): `{"/path/**": "flags"}`
- Empty rules list = full pass-through (inherits user's permissions with no restrictions)
- No matching rule for a path = denied (treated as 404)
- `"crudlify"` = no restrictions from the key on this path (user's normal permissions apply)
- `"--------"` = deny all operations on this path
- Flags are: `c`reate, `r`ead, `u`pdate, `d`elete, `l`ist, `i`nvoke, `f`unctions (deploy), `y` (configure)
- A `-` in any position means that operation is denied by the key

### Permission Resolution

Effective permission = `key_rule_match & user_resolved_permission`. The key can only subtract.

---

## API Surface

### Self-Service Endpoints

**Create key:**
```
POST /api-keys
Authorization: Bearer <token>
Content-Type: application/json
```

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
| `label` | string | No | Human-friendly key name |
| `expires_in_days` | integer | No | Days until expiry (default: 730, max: 3650) |
| `rules` | array | No | Path→permission rules (default: empty = full pass-through) |

Response: `201 Created`

```json
{
  "key_id": "a1b2c3d4-...",
  "key": "aeor_k_a1b2c3d4..._...",
  "user_id": "e5f6a7b8-...",
  "label": "MacBook sync client",
  "expires_at": 1839024000000,
  "rules": [...],
  "created_at": 1776208000000
}
```

The `key` field (plaintext) is returned once and never again.

Any authenticated user can create keys for themselves. Root can additionally include `"user_id": "..."` to create keys for other users.

**List own keys:**
```
GET /api-keys
```

Returns all non-revoked, non-expired keys for the calling user. Never includes key hash or plaintext.

**Revoke own key:**
```
DELETE /api-keys/{key_id}
```

A user can revoke their own keys. Root can revoke anyone's.

### Admin Endpoints (Unchanged)

`/admin/api-keys` routes remain for root to manage all users' keys.

---

## Token Exchange Changes

When an API key is exchanged for a JWT at `POST /auth/token`:

### JWT Gets `key_id` Claim

```rust
pub struct TokenClaims {
    pub sub: String,              // user_id
    pub iss: String,              // "aeordb"
    pub iat: i64,
    pub exp: i64,
    pub key_id: Option<String>,   // API key ID (new)
    pub scope: Option<String>,    // reserved for future use
    pub permissions: Option<Vec<String>>, // reserved for future use
}
```

If the JWT was issued from an API key, `key_id` is set. If issued from password/magic link, `key_id` is `None`.

### JWT Expiry

`exp = min(jwt_default_expiry, key.expires_at)`. A JWT never outlives its source key.

### Validation at Exchange

All strict, all reject:
- Key not found → reject
- Key revoked → reject
- Key expired → reject
- Key hash doesn't match → reject
- Key's user_id not an active user → reject
- Any field missing/corrupt → reject

---

## Permission Middleware Flow

Updated flow for requests with a JWT that has `key_id`:

1. **auth_middleware** — verify JWT signature (EdDSA, in-memory, no DB hit)
2. **permission_middleware:**
   a. Extract `user_id` from `sub`, `key_id` from claims
   b. If `key_id` is present:
      - Load `ApiKeyRecord` from cache (LRU+TTL, same pattern as group cache)
      - If key expired → 401
      - If key revoked → 401
      - If key record missing/corrupt → 401
      - Match request path against key's `rules` (first match wins)
      - If no rule matches → 404
      - If rule's `permitted` doesn't include the operation → 404
   c. Resolve user's normal permissions (groups + path-level, cached)
   d. If user's permissions allow → proceed
   e. If not → 404

**All denials from key restrictions or user permissions return 404, not 403.** The only 403/401 responses come from the auth layer itself (bad token, expired token, revoked key).

### API Key Cache

Same pattern as the existing `GroupCache` — LRU + TTL. Keyed by `key_id`. Invalidated on revoke.

---

## Directory Listing Filtering

When a request has an active API key with rules:

- **Default listing:** for each child entry, check its path against key rules. If denied (no matching rule, or rule's flags don't include `l`) → omit from response.
- **Recursive listing:** same per-entry filter. Don't recurse into directories the key can't list — prune denied branches early.
- **Query results:** exclude files the key can't read from result sets.
- **File history:** if the key can't access the path → return empty history array.

Filtering happens in the HTTP handler (serialization layer), not in the engine. The engine remains permission-agnostic.

---

## Symlink Interaction

Both the symlink path AND the resolved target path are checked against the key's rules:

1. Can the key access the symlink's own path? No → 404 (symlink doesn't exist for you)
2. Can the key access the resolved target path? No → 404 (acts like a dangling symlink)

**Specific behaviors:**
- Symlink at allowed path → allowed target → returns content
- Symlink at allowed path → denied target → 404
- Symlink at denied path → 404 before resolution even happens
- Symlink chain where any intermediate hop is denied → 404
- `?nofollow=true` on allowed symlink → returns symlink metadata (target path visible, but following it checks target permissions)
- Directory listing: symlink shown if key can access the symlink's path. Target check only happens on follow.

---

## Serialization Changes

The `ApiKeyRecord` serialization must include the new fields (`expires_at`, `label`, `rules`). The storage format needs to be backwards-compatible or handle migration:

- **New keys:** serialized with all fields
- **Old keys** (pre-enhancement): when deserialized without `expires_at`/`rules`, the record is treated as **invalid and rejected**. This forces re-creation of old keys, which is acceptable since this is a security boundary — we don't grandfather insecure defaults.

---

## Testing

### Unit Tests

**`api_key_rules_spec.rs`:**
- Rule matching: first-match-wins ordering
- Glob patterns: `**`, `*`, specific paths
- Permission flag parsing: `crudlify`, `-r--l---`, `--------`
- Permission intersection with user permissions
- Empty rules = full pass-through
- No matching rule = denied
- Expired key rejection (at exchange and at middleware)
- Revoked key rejection
- Corrupt/missing fields = reject
- Expiry clamping: > 3650 days → clamped to 3650
- Default expiry: no `expires_in_days` → 730 days

**`api_key_self_service_spec.rs`:**
- Non-root user creates own key with label and rules
- Key stored with correct fields
- Default expiry applied when not specified
- Max expiry enforced
- User lists own keys only (can't see others')
- User revokes own keys only
- Root creates keys for other users
- Root revokes anyone's keys
- Root lists all keys

### HTTP Integration Tests

**`api_key_scoped_http_spec.rs`:**
- POST /api-keys creates scoped key
- Exchange scoped key for JWT → JWT has key_id claim
- Scoped key: allowed path returns content
- Scoped key: denied path returns 404 (not 403)
- Scoped key: directory listing filters denied entries
- Scoped key: recursive listing prunes denied branches
- Scoped key: query results exclude denied files
- Expired key → 401 on token exchange
- Revoked key → 401 on token exchange
- Label and rules returned in key listing
- Malformed rules → 400 on creation
- expires_in_days > 3650 → clamped
- Missing target field in rules → 400

**Symlink + scoped key tests:**
- Symlink at allowed path → allowed target → content returned
- Symlink at allowed path → denied target → 404
- Symlink at denied path → 404
- Chain with denied intermediate hop → 404
- nofollow on allowed symlink to denied target → metadata visible
- Directory listing: symlink shown/hidden based on symlink path access

### E2E Curl Tests

Against a running instance:
1. Create a user with specific group permissions
2. User creates a scoped API key via `/api-keys`
3. Exchange key for JWT at `/auth/token`
4. Access allowed path → content
5. Access denied path → 404
6. List directory → denied entries filtered
7. Create symlink at allowed path → denied target → follow returns 404
8. Revoke key → subsequent exchange fails
9. Create key with default expiry → verify expires_at is ~2 years out
10. Verify root can list/revoke the user's keys via `/admin/api-keys`

---

## Migration

Old `ApiKeyRecord` entries (without `expires_at` and `rules`) will fail strict deserialization. This is intentional — old keys are invalidated and must be re-created. The `emergency-reset` command continues to work as before (creates a new root key with the new format, full permissions, 10-year expiry).

---

## Out of Scope

- OAuth2 / OIDC integration
- Per-key rate limiting
- Key usage analytics / audit log
- Automatic key rotation reminders
- `permissions` JWT claim usage (reserved for future)
