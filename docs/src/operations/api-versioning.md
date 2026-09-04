# API Versioning Policy

## Current State

AeorDB 0.9.x is pre-1.0. The current HTTP API is not prefixed with `/v1/`,
and the sync wire protocol carries no explicit version header. Existing clients
still depend on the documented unprefixed contracts, so current behavior and
compatibility windows must be treated as real even before 1.0.

This document records the policy that will apply once beta opens.

## HTTP API

### URL prefixing
Beta-and-later HTTP routes will be prefixed with `/v1/`. Unprefixed routes will continue to serve `/v1/` semantics for one minor release after the prefix lands (a grace window), then be removed.

### Version negotiation
- Clients indicate desired version by URL prefix (`/v1/files`, `/v2/files`).
- An `Accept: application/vnd.aeordb.v2+json` header is also accepted; the URL prefix wins if both are present.

### Breaking changes
A change is breaking if any of the following occurs:
- A field is renamed or its type changes.
- A field becomes required where it was optional, or vice versa.
- An error code or status code changes for a given input.
- A default value changes.

Breaking changes go to the next major version. Non-breaking additions (new optional fields, new endpoints) ship in the current major.

### Deprecation
- A deprecated endpoint returns `Deprecation: true` and `Sunset: <RFC 1123 date>` headers.
- A deprecated endpoint must remain functional for **6 months** after the deprecation header first ships, or one major version, whichever is longer.
- After sunset: the endpoint returns 410 Gone.

### Error format when a client sends an unknown version
- Unknown `/vN/` prefix: 404 with body `{"error": "unknown API version: vN", "supported": ["v1"]}`.
- Unknown `Accept` version header: 406 Not Acceptable with the same body shape.

## Sync Wire Protocol

The peer-to-peer sync protocol is more sensitive than the public HTTP API because both endpoints must agree on the wire format byte-for-byte.

### Version handshake
- The first request a peer makes on a new connection (typically `/sync/state` or `/sync/handshake` if introduced) includes a `X-AeorDB-Sync-Version: 1` header.
- A peer that doesn't recognize the version replies 426 Upgrade Required with the supported range.

### Version coupling to peer state
- A peer's `last_sync_version` is recorded with its peer config so an old peer that comes back online doesn't mistakenly speak a newer protocol.
- Joining the cluster (`/sync/join`) records the joining peer's sync version. The cluster refuses joins from peers using a sync version newer than the responding node.

### Breaking sync changes
Treated as cluster-wide upgrades:
1. Operators deploy the new build to **one** peer first.
2. That peer continues to speak the old sync version with other peers until all peers are upgraded.
3. Once every peer reports the new version in its heartbeat, the cluster transitions to the new sync protocol.
4. The old version stays supported for one release for rollback.

## Format Version (On-Disk)

The current service selects the v3 compatibility format, including its
validated A/B header and single-file KV/WAL/hot-tail layout. The binary also
contains a capability-admitted v4 format and side-by-side migration substrate;
ordinary startup does not reinterpret v3 bytes as v4.

### Open-time behavior
- Wrong, truncated, ambiguous, or unsupported header/format evidence fails
  closed rather than guessing a version.
- Binary replacement must pass the read-only deployment/capability gate before
  the candidate opens an existing database.
- A v4 destination is a separate verified file until the journaled cutover
  state machine selects it; migration workspaces are never service authority.

### Migration policy
- `aeordb migrate-v4` provides versioned, bounded, side-by-side shadow creation
  and complete verification for an offline v3 copy. It never rewrites the
  source or activates the destination.
- There is no public `cutover` command or HTTP migration route and no automatic
  v3-to-v4 activation. Cutover, rollback, and explicit acceptance remain
  separately gated; an in-place byte rewrite is not an acceptable policy.
- See [V3-to-V4 Migration and Cutover](./migration.md) for the current release
  boundary.

## Library API (Rust crate)

The `aeordb` crate follows Semver:
- Major bump: any breaking change to a public function signature, public struct field, or public trait.
- Minor bump: new public items, deprecations.
- Patch bump: bug fixes, internal changes.

## See Also

- [Threat Model](./threat-model.md)
- [Storage Engine](../concepts/storage-engine.md) — on-disk format details
