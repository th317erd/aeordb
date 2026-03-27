# HTTP Server & Authentication

**Parent:** [Master Plan](./master-plan.md)
**Status:** In Design

---

## HTTP Server: axum

AeorDB's external interface is HTTP(S), powered by axum (built on hyper + tokio + tower).

**Why axum:**
- Built on hyper — maximum HTTP performance
- Tower middleware ecosystem — clean layered architecture for auth, logging, rate limiting
- Maintained by the Tokio team — aligned with our async runtime
- Clean ergonomics without heavy abstraction
- Strong Rust ecosystem convergence — most libraries target axum/tower

### Server Architecture

```
Client request (HTTPS)
       ↓
  TLS termination (rustls or native-tls)
       ↓
  axum router
       ↓
  tower middleware stack:
    → request logging / tracing
    → rate limiting
    → authentication (JWT validation)
    → permission checking
       ↓
  route handler
       ↓
  function execution / admin operation
       ↓
  response
```

### Route Structure

```
# Authentication
POST   /auth/token                              → API key → JWT exchange
POST   /auth/magic-link                         → request magic link email
GET    /auth/magic-link/verify?code=...         → verify magic link → JWT
POST   /auth/refresh                            → refresh JWT

# Admin / Management
POST   /admin/api-keys                          → create API key
DELETE /admin/api-keys/:key_id                  → revoke API key
GET    /admin/api-keys                          → list API keys
POST   /admin/users                             → create user
GET    /admin/health                            → health check
GET    /admin/metrics                           → observability metrics

# Database Operations (function invocation)
POST   /:database/:schema/:table/:function      → invoke function
PUT    /:database/:schema/:table/_deploy         → deploy function
DELETE /:database/:schema/:table/:function       → remove function
GET    /:database/:schema/:table/_functions      → list deployed functions

# Raft (node-to-node, internal)
POST   /_raft/append-entries                    → Raft log replication
POST   /_raft/vote                              → Raft leader election
POST   /_raft/snapshot                          → Raft snapshot transfer
```

---

## Authentication

API-first, token-based authentication. Three flows:

### 1. API Key → JWT (Programmatic / Service-to-Service)

For automated access, CI/CD, service-to-service communication.

```
POST /auth/token
Content-Type: application/json

{ "api_key": "aeor_k_a1b2c3d4e5f6..." }

→ 200 OK
{
  "token": "eyJhbGciOiJFZDI1NTE5...",
  "token_type": "Bearer",
  "expires_in": 3600,
  "refresh_token": "aeor_r_..."
}
```

**API Key properties:**
- Prefixed: `aeor_k_` for identification
- Stored hashed (argon2id) in the database — never stored in plaintext
- Scoped: each key has associated roles/permissions
- Revocable: can be deleted via admin endpoint
- Created via CLI (`aeordb api-key create`) or admin API

### 2. Magic Link → JWT (Human Users, Passwordless)

For human users accessing the database via a UI or admin panel.

**Step 1: Request magic link**
```
POST /auth/magic-link
Content-Type: application/json

{ "email": "user@example.com" }

→ 200 OK
{ "message": "If an account exists, a login link has been sent." }
```

**Step 2: Verify magic link**
```
GET /auth/magic-link/verify?code=abc123def456

→ 200 OK
{
  "token": "eyJhbGciOiJFZDI1NTE5...",
  "token_type": "Bearer",
  "expires_in": 3600,
  "refresh_token": "aeor_r_..."
}
```

**Magic link properties:**
- Code is a cryptographically random token (32+ bytes)
- Stored hashed in the database
- Short-lived: expires after 10 minutes
- Single-use: consumed on verification
- Email delivery mechanism is configurable (plugin/webhook — not a core engine concern)
- Response on request is always the same (prevents email enumeration)

### 3. JWT Refresh

```
POST /auth/refresh
Content-Type: application/json

{ "refresh_token": "aeor_r_..." }

→ 200 OK
{
  "token": "eyJhbGciOiJFZDI1NTE5...(new)...",
  "token_type": "Bearer",
  "expires_in": 3600,
  "refresh_token": "aeor_r_...(new)..."
}
```

**Refresh token properties:**
- Longer-lived than JWT (e.g., 30 days)
- Stored hashed in the database
- Rotated on use (old refresh token is invalidated, new one issued)
- Revocable via admin API

---

## JWT Structure

### Algorithm

Ed25519 (EdDSA) — fast, small signatures, no RSA bloat. The signing key is generated on first database initialization and stored encrypted in the database metadata.

### Claims

```json
{
  "sub": "user_id_here",
  "iss": "aeordb",
  "iat": 1711234567,
  "exp": 1711238167,
  "roles": ["admin", "read_only"],
  "scope": "/mydb/public/*",
  "permissions": {
    "tables": ["users", "orders"],
    "operations": ["read", "write", "deploy"]
  }
}
```

**Key claims:**
- `sub` — user or API key identifier
- `scope` — hierarchical path scope (inherits the function scoping model)
- `roles` — role-based access control
- `permissions` — fine-grained operation permissions
- `exp` — short expiry (1 hour default)

### Validation

JWT validation is **stateless** — no database lookup per request. The server validates:
1. Signature (Ed25519)
2. Expiry (`exp` claim)
3. Issuer (`iss` claim)
4. Scope matches the requested path

This means auth adds near-zero latency to every request.

---

## Per-Cell Permissions — Rules as WASM Plugins

**Status:** Early Design

The vision: permissions enforced at the cell level, not just table or row level. A user might be able to see a row in the `users` table but NOT the `email` column for users they don't own.

### Mechanism: Same Plugin Interface as Queries

Rules are WASM/native plugins — the exact same interface as query functions. A rule is just a function that returns `allow/deny` instead of data. No separate permission system. Same deployment model, same sandboxing, same hierarchy.

This keeps the architecture consistent: **everything is a plugin.**

| Concern | Implementation |
|---|---|
| Queries | WASM/native plugins returning data |
| Rules | WASM/native plugins returning allow/deny |
| Storage backends | Trait implementations |

### Deployment

Rules are deployed to the hierarchy like any other function, with a `type: rule` designation:

```
PUT /mydb/public/users/_deploy
{
  "plugin": "<base64 .wasm>",
  "type": "rule",
  "name": "email_privacy",
  "triggers": ["read", "write"],
  "columns": ["email", "phone"]
}
```

### Execution

The SDK enforces rules transparently at the interface boundary:
1. A query function accesses a cell (e.g., reads `email` on the `users` table)
2. The SDK checks for applicable rules at this scope and all parent scopes
3. Each matching rule plugin is executed with the current context (user from JWT, operation, target cell)
4. If any rule returns `deny`, the cell is either redacted or the request is rejected
5. Rules inherit downward through the hierarchy — a rule at `/database/` applies to everything

### Rule Plugin Interface

```rust
// The context provided to every rule invocation
struct RuleContext {
    user: UserClaims,         // from JWT: user_id, roles, scope
    operation: Operation,      // read, write, delete
    database: String,
    schema: String,
    table: String,
    column: String,
    row: RowAccessor,         // read access to the current row
}

enum RuleDecision {
    Allow,
    Deny,
    Redact,                   // allow the row but redact this cell's value
}

// What a rule plugin implements
fn evaluate(context: &RuleContext) -> RuleDecision;
```

### Example: Email Privacy Rule

```rust
fn evaluate(context: &RuleContext) -> RuleDecision {
    if context.operation == Operation::Read {
        // Only the owning user can see the email
        if context.row.get("user_id") != context.user.id {
            return RuleDecision::Redact;
        }
    }
    RuleDecision::Allow
}
```

### Open Questions

- [ ] How do rules compose when multiple apply to the same cell? (most restrictive wins? priority ordering?)
- [ ] Performance impact of rule evaluation per cell access — caching strategy needed
- [ ] Can admin roles bypass rules? (probably yes, but should be explicit)
- [ ] Rule versioning and hot-reload (same as query functions)
- [ ] How do rules interact with the Raft replication layer? (rules are replicated as deployed artifacts, evaluation is local)
- [ ] Deny vs Redact behavior — who decides? The rule? The admin? Configurable?

---

## Dependencies

| Crate | Purpose | License |
|---|---|---|
| `axum` | HTTP framework | MIT |
| `tower` | Middleware | MIT |
| `hyper` | HTTP implementation | MIT |
| `tokio` | Async runtime | MIT |
| `rustls` | TLS | MIT/Apache-2.0 |
| `jsonwebtoken` | JWT encode/decode | MIT |
| `argon2` | API key hashing | MIT/Apache-2.0 |
| `ed25519-dalek` | JWT signing | BSD-3-Clause |
| `rand` | Cryptographic randomness | MIT/Apache-2.0 |

All MIT or permissive. No license conflicts.

---

## Problems Addressed

From [Why Databases Suck](../docs/why-databases-suck.md):
- **#9 Observability is awful** — HTTP gives us standard tooling (logs, traces, metrics) for free
- **#12 Testing and local dev is painful** — `curl` is your database client. No special drivers.

From [Master Plan](./master-plan.md) Design Principles:
- **Zero-ceremony DX** — HTTP is universal. Every language speaks it.
- **Explicit over implicit** — Auth flows are clear, token scopes are visible, permissions are inspectable.
