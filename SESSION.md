# Session handoff — 2026-05-23 → 2026-05-27

This document covers the work done across a multi-day session ending
2026-05-27 18:15 MST. It's a handoff to the next assistant, not a user
log. Goal: enough context to resume any thread without re-reading the
full transcript.

## TL;DR — current state

- **FS-Server1** (`files.taraani.org`) is running the latest aeordb
  binary deployed 2026-05-27 ~16:42. `systemctl is-active aeordb` →
  `active`. nginx terminates TLS via the wildcard `*.taraani.org`
  cert; reverse-proxies `/` to `127.0.0.1:6830`. systemd unit at
  `/etc/systemd/system/aeordb.service`. Binary at
  `/opt/aeordb/bin/aeordb`. DB at
  `/mnt/storage/aeordb/files.taraani.org.aeordb` (ZFS raidz2, 6 disks).
- **Local test DB** at `/home/wyatt/Documents/AeorDB Tests/main.aeordb`
  (may or may not be running — check `pgrep -fa "aeordb start.*main.aeordb"`).
- **Three platform binaries** in
  `/home/wyatt/Projects/aeordb-workspace/aeordb-www/downloads/`:
  `aeordb-linux-x86_64`, `aeordb-macos-aarch64`,
  `aeordb-windows-x86_64.exe`. All v0.9.5. Built 2026-05-26.
- **No uncommitted work** in aeordb or aeordb-web-components. There's
  an unrelated unstaged `aeor-split-button.css` in
  `aeor-web-components` — not from this session.

## Repos in play

| repo | host path | role |
|---|---|---|
| `aeordb` | `/home/wyatt/Projects/aeordb-workspace/aeordb/` | engine, CLI, portal HTML/JS sources |
| `aeordb-web-components` | `/home/wyatt/Projects/aeordb-workspace/aeordb-web-components/` | DB-aware portal components (file browser, login, keys page, admin page) |
| `aeor-web-components` | `/home/wyatt/Projects/aeor-web-components/` | Generic element-builder DSL + low-level UI primitives (modal, info-box, toast, etc.) |
| `aeordb-client` | `/home/wyatt/Projects/aeordb-workspace/aeordb-client/` | Tauri desktop client. Touched lightly this session (not the focus). |
| `aeordb-www` | `/home/wyatt/Projects/aeordb-workspace/aeordb-www/` | Marketing site / download host. Used as drop point for release binaries in `downloads/`. |

aeordb's `aeordb-lib/src/portal/aeor/` and `…/portal/shared/` are now
**symlinks materialized at build time by `aeordb-lib/build.rs`** —
not tracked in git, not authored by hand. See "Build system changes"
below.

## Remote hosts

| host | role | tool |
|---|---|---|
| `FS-Server1` | Production-ish AeorDB at `files.taraani.org`. Passwordless sudo. `/opt/aeordb/bin/aeordb`, `/mnt/storage/aeordb/...`. nginx, certbot dns-cloudflare for wildcard. | ssh |
| `wyatt-mac` | Apple Silicon dev box. Mirrors `~/Projects/` layout. Used for Mac release builds. | ssh |
| `win11vm` | Windows 11 VM. Rust 1.95 with msvc toolchain. Mirrors `%USERPROFILE%/Projects`. PowerShell + Git. Used for Windows release builds. | ssh (cmd.exe / PowerShell) |

## The user's running test API key (FS-Server1)

```
aeor_k_76fd15e89a8648ab_91260245597038264a088bc4807a4e775ede3b7d0a3aea4a13e83778df8d9ee8
```

This is the root key minted when the FS-Server1 DB was last recreated
(2026-05-25 19:19). Other keys you may see in scrollback
(`aeor_k_01c86be8…`, `aeor_k_e500e84e…`) are from earlier wipes and
DON'T work against the current DB. User has reported the older keys
returning "That API key didn't work" — that's expected.

## Major themes from this session

### 1. Stats counters meant the wrong thing (fixed)

Files / Directories counter on the dashboard showed
`KvSnapshot.count_by_type(KV_TYPE_FILE_RECORD)` — total KV entries
including every superseded revision. User's tree had 255 live files
and 12 directories, dashboard reported 12,940 files / 5,816
directories (~50× / ~484×).

**Fix** (commit `01f2a27`):
- New `count_live_tree(engine)` walker in
  `aeordb-lib/src/engine/directory_listing.rs` walks HEAD's root and
  returns `(files, dirs)`.
- `EngineCounters::initialize_from_kv` uses it for `files` /
  `directories`. Chunks/snapshots/forks/symlinks still come from the
  KV-type-count path (no revision semantics for those).
- New `KvSnapshot::count_by_type(type) -> usize` (O(1)) so we don't
  clone the type-index BTreeMap just to take a length.
- `StatsCounts` gains `file_revisions` and `directory_revisions`
  computed on each `/system/stats` request.
- Dashboard (`dashboard.mjs`) shows the live count as primary value
  with `"N revisions (incl. history)"` as a subtitle when revisions
  exceed live count.

### 2. Engine internal paths weren't all prefixed with `.aeordb-` (fixed)

User reported `/.config/indexes.json` appearing in their DB. Audit
found nine sites still writing or reading unprefixed paths.

**Fix** (commit `b6fd5b0`):
- `aeordb-cli/src/commands/start.rs:255` — default global index
  config was writing to `/.config/indexes.json` on every startup.
- `task_worker.rs:203`, `directory_ops.rs:1637` — reindex /
  compression-detect path formatters had an unprefixed branch.
- `engine_routes.rs:436-439` — auto-reindex no longer accepts the
  unprefixed form.
- `conflict_store.rs` — `/.conflicts/...` → `/.aeordb-conflicts/...`
- `indexing_pipeline.rs:533-535` — `.logs/` → `.aeordb-logs/`
- `index_store.rs:640-660,797-805` — `.indexes/...` →
  `.aeordb-indexes/...`
- `directory_ops.rs:30-33` — `is_internal_path` checked `.logs`
  (wrong) → `.aeordb-logs` (right).
- 11 spec test helpers had the same trailing-slash bug copy-pasted.

Existing FS-Server1 DB still has the orphan `/.config/indexes.json`
from prior buggy runs — engine doesn't recreate it.

### 3. Default index config enriched (commit `ce649ab`)

Was metadata-only (`@filename`, `@hash`, `@size`, …). Now also
indexes:
- `text` (trigram) — full-text body from native parsers
- `title` (string + trigram)
- `metadata.format` (string)
- `metadata.duration` (f64, 0–86400)

These show up on every directory because each indexed (field,
strategy) pair becomes one `<dir>/.aeordb-indexes/<field>.<strategy>.idx`
file. With ~10 directories × 14 strategies, you get ~140 internal
files just from indexing. Documented inline.

### 4. Schema-version dispatch on every stored entity

User flagged that the migration story was undefined. Audit found nine
entity types with no version-dispatch on read.

**Fix** (commit `bf550a8` + follow-up `d3062f2`):
- New `aeordb-lib/src/engine/schema_version.rs` with
  `read_json_version` / `write_json_with_version` helpers and a
  `JsonVersioned` trait.
- Macro `impl_json_versioned_v0!(T)` for entities that have only a v0
  format today.
- Hand-written versioned `deserialize` on User, Group, PathPermissions,
  PathIndexConfig. Macro for ApiKeyRecord, MagicLinkRecord,
  RefreshTokenRecord, PeerConfig, PeerSyncState.
- `Vec<PeerConfig>` wrapped in a `PeerConfigList` struct so the
  envelope is an object (JSON arrays can't carry `$v`).
- DeletionRecord, BTreeNode: new `version: u8` param; dispatch via
  `match`.
- FieldIndex (binary file): 1-byte version prefix at offset 0.
- `deserialize_child_entries` had a silent v0 fallback —
  unconditionally hit `deserialize_child_entries_v0`. Now errors with
  `InvalidEntryVersion`.
- `DiskKVStore::open` now takes a `kv_block_version` parameter and
  validates against `CURRENT_KV_BLOCK_VERSION = 1`. NVT::deserialize
  dispatches on its internal version byte.

The pattern for future migrations is:

```rust
match version {
    0 => Self::deserialize_v0(data, ...),
    1 => Self::deserialize_v1(data, ...),     // new on-disk format
    _ => Err(InvalidEntryVersion(version)),
}
```

### 5. Idle engine was keeping HDDs spinning (fixed TWICE)

User reported HDDs ticking every few seconds on FS-Server1 even with
zero user activity.

**First fix** (commit `21873d7`): The 100ms hot-buffer flush timer
was unconditionally calling `writer.sync_data()` (fdatasync) and
header-rewriting every tick. Added an early-exit when neither buffer
had content.

**Bug in that fix** (commit `0e43fed`): The gate was
`hot_buffer_len() > 0 || write_buffer_len() > 0`. write_buffer's
lifecycle is independent — `kv.insert()` adds to BOTH buffers, but
hot_buffer clears at 512 entries (and on each tick if non-empty)
while write_buffer only clears at `WRITE_BUFFER_THRESHOLD` (much
higher) or explicit flush. So after any past activity,
`hot_buffer.empty && write_buffer.len() > 0` is the normal idle
state. The OR-gate kept the timer firing forever.

Fix: gate ONLY on `hot_buffer_len() > 0`. That timer's job is
hot-tail durability; write_buffer has its own threshold. Verified on
FS-Server1: 10s strace → zero fdatasyncs, zpool iostat → 0 ops on
storage pool.

### 6. Refresh-token leak from poll-heavy dashboard (fixed)

User noticed `files` counter ticking up by 1 every ~15s on the local
test DB despite zero activity. Each tick was a new
`/.aeordb-system/refresh-tokens/<hash>` entry.

Root cause: dashboard polls `/system/stats` every 15s, the client
creates a fresh `RemoteClient` per call with an empty JWT cache, so
each call does `POST /auth/token`. The engine *unconditionally*
minted and persisted a refresh token alongside every JWT.

**Engine fix** (commit `bad4eb8`):
- `AuthTokenRequest` gains `include_refresh: bool` (default false).
  Refresh token only persisted when caller opts in. `refresh_token`
  field only present in response when opted in.
- `DEFAULT_EXPIRY_SECONDS` was 3600 (1 hour) — drifted from the
  7-day design intent. Bumped to `7 * 24 * 3600` = 604800. Doc
  strings on `--jwt-expiry` + `auth.jwt_expiry_seconds` updated.

**Client fix is OPEN** (tracked in `bot-docs/bug-reports/2026-05-26-auth-token-polling-leak.md`):
- Move JWT cache out of per-handler `RemoteClient` instances and into
  `AppState`, keyed by `connection_id`.
- Replace inline `POST /auth/token` in `sync/pull.rs:397` with the
  cached path.
- Wire `invalidate_token` on 401 (kills the `dead_code` warning).
- Default `include_refresh: false` everywhere; `true` only in
  interactive browser-login flow.

User said they'd do the client-side fix. **As of session end, that
fix hasn't been confirmed merged** — but it's the user's other repo
and out of our scope.

### 7. Build system overhaul

Three interlocking changes:

**a. Relative symlinks** (commit `35ea463`): The previously-committed
absolute paths (`/home/wyatt/Projects/...`) broke every non-Linux
clone. Changed to relative paths but those broke on Windows because
git checks symlinks out as plain text files by default.

**b. build.rs takes over** (commits `be0667a`, `c59712d`, `6f70c2f`):
- `aeordb-lib/build.rs` is new. Materializes the symlinks at build
  time. On Unix: real symlinks with a relative target. On Windows:
  NTFS junctions (no admin/Developer Mode required — `mklink /J`).
- `portal/aeor` and `portal/shared` are gitignored — they never
  appear in commits.
- Searches up to 12 ancestors for the sibling repo directories
  (`aeor-web-components`, `aeordb-web-components`), so it works for
  both the Linux nested layout (`aeordb-workspace/aeordb/`) and the
  flat Mac/Windows layout (`Projects/aeordb/` sibling-style).
- Panics with a clear "you need to clone X" message when a sibling
  is missing, instead of letting `include_str!` produce 53 errors.

**c. Cross-platform builds verified** (2026-05-26):
- Linux: 32s, 23.2 MB
- Mac aarch64 (`wyatt-mac`): 30s, 24 MB
- Windows x86_64-msvc (`win11vm`): 61s, 22.5 MB

All three deposited in `aeordb-www/downloads/`. CI config
(`.github/workflows/ci.yml.disabled`) is still disabled — re-enable
with `macos-latest` matrix when ready.

### 8. Portal cache headers (commit `a0978e7`)

Engine portal asset routes had no `Cache-Control`/`ETag`/`Last-Modified`
at all. After a deploy, browsers (esp. with module-map caching) could
hold stale JS indefinitely.

Fix: new `asset_response` helper that all three handlers
(`portal_asset`, `portal_shared_asset`, `portal_aeor_asset`) route
through. Sends `Cache-Control: no-cache, must-revalidate` and an
ETag of the form `"<pkg-version>-<startup-uuid-16hex>"`. Per-process
nonce changes on every engine restart, so deploys auto-invalidate
caches the next time browsers revalidate. Conditional 304 fast path
when `If-None-Match` matches.

### 9. Login + Keys-page UI polish

- **Login error rendering** (commit `2b66ec8` on aeordb-web-components):
  was throwing the raw JSON envelope as `error.message`. Now parses
  and surfaces `parsed.error` or `parsed.message`.
- **Keys-page post-create modal** (commits `e3d8ed6`, `ed7acb9`):
  inline "Done" button removed from body, footer "Cancel + Creating…"
  replaced by single "Done", `aeor-info-box warning` used (yellow ⚠
  triangle), margin under the box, `disable-overlay-close`
  attribute set so accidental clicks/Escape don't destroy the only
  copy of the key.
- **`disable-overlay-close` attribute** (commit `cc5ace5` on
  aeor-web-components): new opt-out on `aeor-modal` that suppresses
  both overlay-click and Escape dismissal. Default behavior unchanged.

### 10. Empty-state guidance for users with no access (commits `5cdedd3`, `091d750`, `d92e1b6`, `1b9b7c1`, `d06ca5e`, `f91de39` on aeordb-web-components)

A freshly-created non-root user with no grants used to land on the
file browser at `/` with a blank screen and no action buttons. Now:

- Detection helper `_isNoAccessState(tab)` in
  `aeor-file-browser-base.js` (line ~770). Non-root + not loading +
  no fetch error + zero entries + zero visible.
- `_renderNoAccessCard(userId)` renders a card with the user's UUID
  and a Copy button.
- Empty-state path in `_renderListingContent` returns the card when
  the helper says so.
- `renderNoTabContent` overrides return the same card for non-root
  users with no tabs open.
- `_renderDirectoryViewFor` returns just `[listing]` (no page header,
  no toolbar) when in no-access state — the breadcrumb and view-mode
  controls are pointless when there's nothing to act on.
- `_updateTabContent` checks for layout transitions and falls into
  the full-rebuild branch when the DOM doesn't match the desired
  state — handles both directions (gaining access → header/toolbar
  appear; losing all shares → guidance card replaces them).

### 11. User-creation workspace bootstrap (commit `42b9b49`)

Create User modal grew a "Grant a personal workspace" checkbox
(default on). Path field auto-suggests `/workspaces/<username>` as
the username is typed; admin can override or leave blank. On submit:

1. `POST /system/users` (existing)
2. If checkbox is on: `POST /files/mkdir` + `POST /files/share` with
   `crudlify` on the new user's UUID.

Best-effort: if mkdir or share fails, surface a warning toast but
don't roll back the user record.

Service / backup / replication users uncheck the box to start with
zero grants.

Documented at `docs/src/concepts/users-and-workspaces.md` (added to
`SUMMARY.md`).

## Other ops that happened this session

- **Swap on FS-Server1**: was `/swapfile` 512 MiB, switched to the
  16 GiB `sdc3` partition that was already formatted but not in
  fstab. `/etc/fstab` updated to use UUID. `/swapfile` removed.
  `vm.swappiness` lowered from 60 → 10 on both FS-Server1 and local
  laptop (`/etc/sysctl.d/99-swappiness.conf`).
- **Cert renewal hook**: `/etc/letsencrypt/renewal-hooks/deploy/reload-nginx.sh`
  added so nginx auto-reloads after certbot DNS-01 renewals. Affects
  all four certs on the host (aeordb.com, mythix.info, taraani.org,
  xenocept.com).
- **systemd unit** committed to repo at `deploy/systemd/aeordb.service`
  + `deploy/systemd/README.md` so future deploys can copy from there.

## Open / known issues

- **Auth client-side polling leak** is NOT fully fixed. Engine side
  shipped (opt-in refresh + 7d JWT). Client side (move JWT cache to
  AppState, kill inline `/auth/token` in `sync/pull.rs`, wire
  `invalidate_token` on 401) is the user's to do.
- **`shared/styles/components.css` is a hand-bundled CSS file**
  in `aeordb-web-components/styles/`. Component CSS from
  `aeor-web-components/components/*.css` is manually copied into it.
  No build step keeps them in sync. We discovered this when the
  `aeor-info-box[warning]` rules were missing — synced them by hand.
  Long-term fix: a build script that concatenates per-component CSS,
  OR have each component self-inject its stylesheet via constructable
  stylesheets.
- **`FileHeader.nvt_version`** is stored on disk but not used for
  dispatch (the NVT carries its own internal version byte that
  `NormalizedVectorTable::deserialize` matches on). Documented as
  bookkeeping. Not blocking.
- **Spec test status unknown**: I made versioning + path-prefix
  changes that touched many spec tests. The release builds compile
  cleanly. I didn't run the full spec suite this session — if you
  want, run `cargo test --workspace --release -- --skip stress` and
  triage.
- **`writes_total` counter and SSE metrics pulse**: the metrics
  pulse was historically passed `counters` only; it now needs
  `engine` too (for the count_by_type calls). Spec tests in
  `aeordb-lib/spec/engine/metrics_pulse_spec.rs` were updated to
  pass `engine.clone()` — 11 call sites changed (verified at the
  time, but flagged here in case anything else surfaces).

## File / module reference

Useful files when working on what was changed:

- `aeordb-lib/build.rs` — symlink materializer
- `aeordb-lib/src/engine/schema_version.rs` — JsonVersioned trait,
  helpers, macro
- `aeordb-lib/src/engine/directory_listing.rs` — `count_live_tree()`
- `aeordb-lib/src/engine/storage_engine.rs::try_flush_hot_buffer` —
  the function with the timer gating
- `aeordb-lib/src/server/portal_routes.rs::asset_response` —
  cache-header path
- `aeordb-lib/src/server/routes.rs::auth_token` — the opt-in refresh
  logic
- `aeordb-lib/src/portal/users.mjs::bootstrapWorkspace` — workspace
  helper
- `aeordb-web-components/components/aeor-file-browser-base.js`:
  - `_isNoAccessState`, `_renderNoAccessCard`, `renderNoTabContent`
    override, `_renderDirectoryViewFor` no-access return,
    `_updateTabContent` layout-transition detection
- `aeordb-web-components/components/aeor-keys-page.js::onPostCreate`
  — the modal post-create swap (warning box + Done footer +
  `disable-overlay-close`)
- `aeor-web-components/components/aeor-modal.js::_onOverlayClick` /
  `_onKeyDown` — `disable-overlay-close` honored

## Operational shortcuts

```bash
# Rebuild + deploy engine to FS-Server1 (after any aeordb change)
cd /home/wyatt/Projects/aeordb-workspace/aeordb
cargo build --release --quiet
scp -q target/release/aeordb FS-Server1:/tmp/aeordb-new
ssh FS-Server1 'sudo systemctl stop aeordb && sudo install -o root -g root -m 0755 /tmp/aeordb-new /opt/aeordb/bin/aeordb && rm /tmp/aeordb-new && sudo systemctl start aeordb'

# Restart local test engine
nohup /home/wyatt/Projects/aeordb-workspace/aeordb/target/release/aeordb \
  start -D "/home/wyatt/Documents/AeorDB Tests/main.aeordb" \
  > /tmp/aeordb-test.log 2>&1 &

# Check what's served (versus what's in the source)
curl -sS https://files.taraani.org/shared/components/aeor-file-browser-base.js | sha256sum
sha256sum /home/wyatt/Projects/aeordb-workspace/aeordb-web-components/components/aeor-file-browser-base.js

# ETag of currently-running engine (changes per restart)
curl -sS -I https://files.taraani.org/shared/components/aeor-file-browser-base.js | grep -i etag

# Standard sync workflow used throughout: dev → main with --no-ff
cd /home/wyatt/Projects/aeordb-workspace/aeordb
git push origin development
git checkout main && git pull origin main \
  && git merge development --no-ff -m "merge: development → main" \
  && git push origin main && git checkout development
```

## User communication preferences

From memory + observed:
- Direct, terse responses. The user doesn't want padding or
  summarize-what-I-just-said.
- Always rebuild + restart the engine after engine changes; don't ask.
- Show file_path:line_number when referring to source.
- Pre-beta: format/wire breaks are free. The user explicitly said
  this multiple times when we changed disk layouts.
- The user trusts you to commit + push + sync to main. They invoke
  that workflow themselves and have you carry it out.
- For multi-file/multi-step tasks: TaskCreate / TaskUpdate is
  expected.
- Stay out of the aeordb-client repo unless explicitly invited
  (the user does that side themselves).

## Last 30 seconds before this handoff

The most recent thing we did before this dump was fix
`_updateTabContent` to do a full rebuild on no-access ↔ has-access
transitions (commit `f91de39` on aeordb-web-components). User
reported that sharing a folder to themselves caused the listing to
update live (SSE working) but the header/toolbar didn't come back.
That's now fixed. Engine on FS-Server1 was rebuilt and redeployed
2026-05-27 ~16:42. ETag at that point was `"0.9.5-0b7b8f510938420e"`.

If the user comes back saying "still no header after sharing," check
that they actually got the new JS — open
`https://files.taraani.org/shared/components/aeor-file-browser-base.js`
and grep for `layoutChanged` (should be present in the new code).
