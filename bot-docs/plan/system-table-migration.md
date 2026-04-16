# System Table Migration to /.system/ — Design Spec

**Date:** 2026-04-16
**Depends on:** Replication v2 (all phases complete)

---

## Overview

Move system table entries (JWT signing key, API keys, users, groups, permissions, config) from loose KV entries into the directory tree under `/.system/`. This makes system data sync automatically through the existing replication protocol. Add a `FLAG_SYSTEM` entry header flag to enforce root-only access at the chunk level.

## Problem

System table entries are currently stored as loose KV entries keyed by BLAKE3 hashes of domain-prefixed strings (e.g. `BLAKE3("::aeordb:config:jwt_signing_key")`). These entries are NOT part of the directory tree — they're not reachable from HEAD. The tree walker and sync protocol never see them.

This means:
- System data doesn't sync between replication peers
- A joining node doesn't receive the signing key, API keys, or user data via sync
- The "client = node" model breaks for system data

## Solution

### 1. Store system data as files under `/.system/`

```
/.system/
    config/
        jwt_signing_key          → raw signing key bytes
    apikeys/
        _registry                → JSON array of key IDs
        {uuid}                   → JSON ApiKeyRecord
    users/
        {uuid}                   → JSON User record
    groups/
        {name}                   → JSON Group record
    permissions/
        {path_hash}              → JSON PathPermissions
    magic-links/
        {code_hash}              → JSON MagicLinkRecord
    refresh-tokens/
        {token_hash}             → JSON RefreshTokenRecord
```

Since these are regular files in the directory tree, they're reachable from HEAD, included in tree walks, and sync automatically.

### 2. FLAG_SYSTEM on entries

Add a system flag to the EntryHeader flags byte:

```rust
pub const FLAG_SYSTEM: u8 = 0x01;
```

When storing ANY entry under `/.system/` (chunks, FileRecords, DirectoryIndex entries), the flag is set in the header.

### 3. System hash domain

Chunks for system entries use the `system::` domain prefix instead of `chunk:`:

```rust
pub fn system_chunk_hash(data: &[u8], algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
    let mut input = Vec::with_capacity(8 + data.len());
    input.extend_from_slice(b"system::");
    input.extend_from_slice(data);
    algo.compute_hash(&input)
}
```

This cryptographically separates system chunks from user chunks. A user cannot craft data that produces a system chunk hash because user chunks always use the `chunk:` prefix.

### 4. Access control

**Root-only access to /.system/:**

- `GET /engine/.system/**` → 404 for non-root (path doesn't exist for you)
- `PUT /engine/.system/**` → 404 for non-root
- `DELETE /engine/.system/**` → 404 for non-root
- `HEAD /engine/.system/**` → 404 for non-root
- Directory listing → `/.system/` omitted for non-root
- `/sync/diff` for non-root → `/.system/` entries excluded from diff
- `/sync/chunks` → entries with `FLAG_SYSTEM` denied for non-root

The check is simple: if the path starts with `/.system/` and `!is_root(&claims.sub)` → 404.

For chunks: if the entry header has `FLAG_SYSTEM` set and the caller is not root → return nothing (indistinguishable from "hash not found").

### 5. Engine-level enforcement

The check should be at the engine route level (in `engine_get`, `engine_head`, etc.) and in the sync handlers. The engine itself (StorageEngine, DirectoryOps) doesn't enforce access — it stores and retrieves whatever it's told. The HTTP layer enforces the access rules.

This matches the existing pattern: the engine is permission-agnostic, the server layer enforces.

---

## Migration Path

### Phase 1: Add FLAG_SYSTEM and system hash domain

- Add `FLAG_SYSTEM = 0x01` to entry_header.rs
- Add `system_chunk_hash()` and `system_content_hash()` functions
- Modify `store_entry` / `store_entry_compressed` to accept an optional flags parameter
- No behavior change yet — just the infrastructure

### Phase 2: Create `/.system/` store/read layer

New module `system_store.rs` that provides the same API as `system_tables.rs` but stores under `/.system/`:

```rust
pub fn store_config(engine, ctx, key, value) → stores at /.system/config/{key}
pub fn get_config(engine, key) → reads from /.system/config/{key}
pub fn store_api_key(engine, ctx, record) → stores at /.system/apikeys/{key_id}
pub fn get_api_key(engine, prefix) → reads from /.system/apikeys/...
pub fn list_api_keys(engine) → lists /.system/apikeys/
// ... etc for users, groups, permissions
```

All writes to `/.system/` paths set `FLAG_SYSTEM` on every entry created (chunks, FileRecords, directories).

### Phase 3: Migrate system_tables callers

Update all code that calls `system_tables.store_api_key()`, `system_tables.get_config()`, etc. to use the new `system_store` module instead. The old `system_tables` module becomes deprecated.

Affected:
- `auth/provider.rs` — JWT signing key storage
- `auth/mod.rs` — bootstrap_root_key
- `server/routes.rs` — API key creation, token exchange
- `server/api_key_self_service_routes.rs` — self-service key management
- `server/admin_routes.rs` — user/group management
- `engine/permission_resolver.rs` — permission loading
- `engine/group_cache.rs` — group membership
- `aeordb-cli/commands/emergency_reset.rs` — root key reset

### Phase 4: Access enforcement

- In `engine_get`, `engine_head`, `engine_delete`, `engine_store_file`: if path starts with `/.system/` and caller is not root → 404
- In directory listing: filter `/.system/` for non-root
- In `/sync/diff`: exclude `/.system/` entries for non-root callers
- In `/sync/chunks`: check `FLAG_SYSTEM` on entry header, deny for non-root
- In `directory_listing.rs`: exclude `/.system/` for non-root

### Phase 5: Remove old system_tables

Once all callers are migrated:
- Remove `system_tables.rs` (or keep as thin compatibility layer)
- Remove the old `::aeordb:` prefixed loose KV entries
- GC will clean up orphaned old-format entries

### Phase 6: Testing

- System data syncs between peers (signing key, API keys, users)
- Non-root cannot read /.system/ via any endpoint
- Non-root cannot fetch system-flagged chunks
- GC handles /.system/ entries correctly
- Conflict resolution works for /.system/ entries (LWW, same as files)
- API key operations work through the new storage layer
- Auth flow works end-to-end with new storage
- Emergency reset works with new storage
- E2E: two nodes sync, JWT from node A works on node B

---

## Security Guarantees

1. **Hash domain separation** — system chunks use `system::` prefix, user chunks use `chunk:` prefix. Cross-domain hash collision requires breaking BLAKE3.

2. **FLAG_SYSTEM on entries** — every entry (chunk, FileRecord, DirectoryIndex) under `/.system/` carries the flag. The sync/chunks endpoint checks this flag before serving.

3. **Path-level deny** — `/.system/**` returns 404 for non-root on ALL HTTP endpoints. Not 403 — 404. No information leakage.

4. **Sync-level filtering** — non-root sync callers never see `/.system/` entries in diff responses. They can't know what system chunks exist, so they can't request them.

5. **Defense in depth** — even if a non-root caller somehow obtains a system chunk hash, the FLAG_SYSTEM check on the chunks endpoint blocks access.

---

## Impact on Existing Features

- **Replication** — system data now syncs automatically. The signing key gap is closed.
- **GC** — `/.system/` is in the directory tree, so entries are automatically marked as live. No special handling needed.
- **Backup/Export** — `/.system/` is included in exports. Root-level backups contain everything.
- **Versioning** — `/.system/` entries are versioned like any other files. Snapshot captures system state.
- **Conflicts** — concurrent system table modifications follow the same conflict resolution rules. API key revocation always wins (existing rule).

---

## Open Questions

1. **Should `/.system/` be included in client-visible snapshots?** A snapshot taken by a non-root user shouldn't expose system data. The snapshot itself is fine (it captures HEAD which includes `/.system/`), but READING the snapshot's `/.system/` entries should still be gated by root access.

2. **Performance** — system_tables currently uses direct KV lookups (O(1) by computed hash). The new approach goes through the directory tree (O(depth)). For `/.system/config/jwt_signing_key` that's depth 3. Probably fine, but should be benchmarked.

3. **Atomic system operations** — creating a user involves storing the user record AND updating group memberships. Currently these are separate KV writes. Under `/.system/`, they're separate file writes. Same atomicity guarantees (or lack thereof). The append-only WAL ensures individual writes are durable, but a crash between two writes leaves partial state. Same as current behavior — no regression.
