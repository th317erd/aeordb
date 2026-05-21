# Threat Model

This document describes the trust boundaries, assets, attacker classes, and mitigations for AeorDB. It is a working document — keep it current as the security surface evolves.

## Trust Boundaries

AeorDB has five distinct trust boundaries. An identity that holds rights on one side of a boundary should not be presumed to hold rights on the other.

| Boundary                | Inner trust                              | Outer trust                             |
|-------------------------|------------------------------------------|-----------------------------------------|
| Peer ↔ Peer             | Cluster signing key holder                | Anyone reachable over the network        |
| Server ↔ Client         | Server process + filesystem               | HTTP clients (browsers, CLIs, services) |
| Root user ↔ Scoped user | Root API key holder                       | Other identities (per-key scoped tokens) |
| Public mode             | Anyone — but writes are forbidden          | Same                                    |
| Auth-required mode      | Authenticated identities                   | Anonymous callers (rejected at edge)    |

## Assets

What an attacker would want to obtain or modify:

1. **The Ed25519 cluster signing key.** Held in `/.aeordb-system/jwt_signing_key`. Holding this key lets an attacker mint JWTs that any peer in the cluster accepts. Exposed by `/sync/join` to a successful (root-authed) caller — see "Compromised peer" below.
2. **Refresh tokens.** Held in `/.aeordb-system/refresh-tokens/` (hashed). A live refresh token mints new JWTs for its subject. Now bound to a specific API key (the issuing key); revoking the key invalidates the chain on next refresh.
3. **Root API keys.** Held in `/.aeordb-system/api-keys/`. Compromise = total cluster access.
4. **Scoped API keys.** Limited to a `(path, operation)` rule set. Compromise = leakage of the data in scope.
5. **File content + metadata.** What users actually store. Confidentiality is via auth-required mode + key scoping. Integrity is via content-addressed hashes (BLAKE3) at the entry layer.

## Attacker Classes

### Untrusted client with valid scoped key
**Capabilities:** Holds a non-root API key with a defined `(path, operation)` scope. Cannot list keys, users, or system state.

**What they should not be able to do:**
- Access paths outside their scope, even via aliases (symlinks, version-history queries, /blobs/{hash}).
- Mint a root JWT or refresh a token bound to a different key.
- Read or write `/.aeordb-system/` or `/.aeordb-config/` directly.

**Mitigations:**
- Per-key `ActiveKeyRules` checked on every request that touches a path.
- `/blobs/{hash}` and `/files/download` now route through the same scope check as `/files/{path}` (closes beta-audit P0).
- `/auth/refresh` validates the issuing key's `is_revoked` and `expires_at` on every refresh, so revoking a leaked key terminates outstanding refresh chains.

### Compromised peer
**Capabilities:** Holds the cluster signing key (acquired via a successful `/sync/join` or by physical access to a node). Can mint JWTs accepted by every peer.

**What they should not be able to do:**
- Be admitted invisibly. Every `/sync/join` is now rate-limited per-IP and audit-logged to `/.aeordb-system/join-audit/`.
- Cause silent data loss in other peers. Sync conflicts go through the LWW conflict store, which preserves losers as snapshots.

**Mitigations:**
- `/sync/join` per-IP rate limit (5/min, 30/hour).
- Audit log at `/.aeordb-system/join-audit/<ts>-<ip>.json` for every join attempt.
- Sync requires `scope: "sync"` JWT, not arbitrary root tokens.

**Residual risk:** A compromised root token + ability to reach `/sync/join` once = one signing-key extraction. The audit log + rate limit raise the cost but don't eliminate it. Encryption-at-rest (separate project) is the long-term mitigation.

### On-path observer
**Capabilities:** Sees raw HTTP between client and server, or between peers.

**What they should not be able to do:**
- Recover content if TLS terminates at the server.
- Replay captured JWTs after they expire.

**Mitigations:**
- AeorDB does not terminate TLS itself; operators should deploy behind a reverse proxy with TLS (caddy, nginx).
- JWTs are short-lived (default 1 hour). Refresh tokens are bound to their issuing key.

**Residual risk:** Without TLS, all the above mitigations are moot — the threat model assumes TLS at the edge.

### Lost-laptop scenario
**Capabilities:** Adversary has the `.aeordb` file plus filesystem access.

**What they should not be able to do:**
- Bypass authentication if the database is encrypted at rest.

**Mitigations:**
- (Future) Full database-level encryption project. Until that lands, treat the `.aeordb` file as containing plaintext content + hashed credentials.

**Residual risk:** Current state. File-level encryption is the next major security project.

## Public-mode vs Auth-required mode

In public mode, all read endpoints are open. Writes are still rejected at the auth middleware. The threat model in public mode is: anyone can read everything; this is acknowledged and intentional.

In auth-required mode, the auth middleware rejects every request without a valid JWT or root key. Scoped keys further restrict the path space.

## Out of Scope

- Side-channel attacks (timing, cache).
- Physical attacks on the host.
- Insider attacks by an operator with shell access.
- Denial-of-service attacks beyond what TLS terminators handle.

## See Also

- [API Versioning Policy](./api-versioning.md)
- [Cluster Operations](./cluster.md)
- [Backup & Restore](./backup.md)
