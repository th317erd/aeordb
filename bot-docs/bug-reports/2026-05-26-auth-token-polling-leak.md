# `/auth/token` polling leak — refresh-token rows accumulate forever

**Status:** engine-side fixed in commit `bad4eb8` (development) /
`2a94027` (main). Client-side fix tracked separately in the
aeordb-client repo.

**Severity:** medium — bounded disk leak, ~25 KB/hour on an idle
dashboard, but no upper limit until the 30-day refresh-token TTL
catches up.

## Observed

On the local test DB the `files` counter rose by ~1 every 15 seconds
even with no user activity. SSE event stream confirmed each tick was
an `entries_created` event for a path like:

```
/.aeordb-system/refresh-tokens/92ddcd212d457ec344863dd956272ab6d533d9c4554fa88bd2966cfa6e5475b7
```

301 bytes per record, 4 records/minute = ~70 KB/hour, ~1.7 MB/day,
~50 MB/month before the first row expired.

## Root cause — engine

`POST /auth/token` (`aeordb-lib/src/server/routes.rs:auth_token`)
unconditionally minted **and persisted** a refresh token alongside
the JWT. There was no opt-out for callers that didn't want one. Three
related drifts compounded the impact:

1. **JWT lifetime drifted to 1 hour.** `DEFAULT_EXPIRY_SECONDS` in
   `aeordb-lib/src/auth/jwt.rs:14` was `3600`. Original design intent
   was 7 days. Any client that re-exchanges the API key when its JWT
   nears expiry was doing it 168× more often than designed.
2. **Refresh-token TTL is 30 days** (`auth/refresh.rs:10`,
   `DEFAULT_REFRESH_EXPIRY_SECONDS = 30 * 24 * 3600`). The hourly
   cleanup cron purges only **expired** rows, so accumulation runs
   for a full month before any GC.
3. **No deduplication or per-(user, key_id) cap.** Every exchange
   gets its own row, even when a non-expired row already exists for
   the same identity.

## Root cause — client (aeordb-client)

The Dashboard polls `/system/stats` every 15 seconds
(`aeor-dashboard.js:139,144`, `setInterval(() => this.fetchStats(),
15000)`). The path is `/api/v1/connections/{id}/proxy/{*path}`.

`RemoteClient::auth_header()` does have a JWT cache
(`remote/mod.rs:155`), but each handler creates a brand-new
`RemoteClient` instance per call via
`RemoteClient::from_connection(...)`. So every handler invocation
starts with an empty cache, calls `exchange_token` → `POST
/auth/token`, gets a fresh JWT — and (before today's engine fix) a
fresh persistent refresh-token row.

Other latent leakers in the same shape:

- `sync/pull.rs:397` bypasses the cache entirely — does an inline
  `POST /auth/token` on every `sync_diff` call. Fires per
  `sync_interval_seconds` (default 60s) per relationship.
- `RemoteClient::invalidate_token` exists but is unused
  (`#[allow(dead_code)]` warning all session). Nothing clears the
  cache on 401 / expiry, so even if it were shared, it'd 401-loop
  after the JWT aged out.

Not affected:

- The connection-health pinger sends the raw API key as Bearer (no
  JWT exchange).
- File-browser browse uses the cached `auth_header`, but the cache
  is per-handler-call, so it still triggers a fresh exchange.

## Fix — engine (already committed)

Commit `bad4eb8`:

- **`DEFAULT_EXPIRY_SECONDS` → `7 * 24 * 3600`** (604,800s, 7 days).
  Updated CLI `--jwt-expiry` help and `auth.jwt_expiry_seconds`
  config doc to match. No upper bound change; operators can still
  override with `--jwt-expiry`.
- **`AuthTokenRequest` gains `include_refresh: bool`**
  (`#[serde(default)]`, so default is `false`). The persistent
  refresh-token row is only created — and the `refresh_token` field
  only appears in the response — when the caller opts in.

API impact: callers that previously relied on receiving a
`refresh_token` field in the response will get back JSON without it
unless they pass `include_refresh: true`. The default is the
narrowly correct behavior: no persistent state for callers that
don't ask for it.

## Fix — client (recommended, in aeordb-client repo)

1. **Move the JWT cache out of `RemoteClient` instances and into
   `AppState`**, keyed by `connection_id`. Sketch:
   ```rust
   // AppState
   pub jwt_cache: Arc<RwLock<HashMap<String, CachedJwt>>>,
   // struct CachedJwt { token: String, exp: i64 }

   pub async fn remote_client_for(state, id) -> RemoteClient {
       // pass Arc<Mutex<Option<String>>> from the cache map
   }
   ```
2. **Replace the inline `POST /auth/token` in `sync/pull.rs:397`**
   with the cached path.
3. **Wire `invalidate_token` to fire on 401** (kills the
   `dead_code` warning at the same time).
4. **Optionally decode JWT `exp`** so the client proactively
   refreshes ~30s before expiry instead of waiting for a 401.
5. **Default `include_refresh: false`** on all `POST /auth/token`
   calls; only set `true` in the browser login flow that actually
   needs a refresh token to survive page reloads.

## Steady-state after both fixes

- Dashboard polls /system/stats every 15s: cached JWT, no re-exchange
  for 7 days, zero refresh-token rows created.
- Sync peers: one `/auth/token` call per 7 days per peer, zero
  refresh-token rows (sync uses API key directly).
- Interactive browser login: one refresh-token row per login,
  expiring in 30 days. Hourly cleanup catches them after expiry.

## Follow-ups worth considering (not done)

- **Per-(user, key_id) refresh-token cap** — collapse multiple
  active rows for the same identity to one, upsert on refresh.
- **Per-key rate limit on `/auth/token`** — exists for invalid keys
  (anti-brute-force) but not for valid ones; a misbehaving client
  could still spam.
- **Shorter `DEFAULT_REFRESH_EXPIRY_SECONDS`** — 30 days is on the
  generous side; many providers use 14 days. Tradeoff is user
  experience (forced re-login frequency) vs. blast radius of a
  leaked refresh token.
