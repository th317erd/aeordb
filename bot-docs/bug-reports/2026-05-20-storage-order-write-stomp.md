# Bug Report — Storage order causes second write to be unreadable

**Filed:** 2026-05-20 by the Xenocept WWW team
**AeorDB version:** `0.9.5` (workspace path dep at `aeordb-workspace/aeordb/aeordb-lib`)
**Severity:** High — silently breaks magic-link auth when two `system_store` writes happen in a single request handler in a specific order.

---

## TL;DR

When a single handler does, in order:

1. `system_store::store_user(engine, ctx, &user)`  — writes `/.aeordb-system/users/{uuid}` + `/.aeordb-system/groups/user:{uuid}`
2. `system_store::store_magic_link(engine, ctx, &record)`  — writes `/.aeordb-system/magic-links/{code_hash}`

…then a subsequent `system_store::get_magic_link(engine, &code_hash)` (from any handler invocation, including the very next HTTP request) returns `Ok(None)` — the magic-link record is **not found**, even though `store_magic_link` returned `Ok(())` and the underlying `.aeordb` file *did* grow in size by the expected number of bytes.

Reversing the order to `store_magic_link` **first**, then `store_user`, makes both writes readable. The fix in our code is purely a reordering — no other changes.

The data on disk is real (the file grew, restarting the process and reading via `list_directory` would presumably find it). The KV / index path that `get_magic_link` traverses cannot resolve the record.

This looks like an index/KV-side bug, not a WAL-side bug — the bytes are there, the lookup just misses them.

---

## Our Setup

A small Rust HTTP server (`xenocept-www-server`, a binary in `xenocept-workspace/xenocept-www/server/`) embeds `aeordb-lib` as a Cargo path dependency:

```toml
[dependencies]
aeordb = { path = "../../../aeordb-workspace/aeordb/aeordb-lib" }
```

At startup we call:

```rust
let (aeordb_router, _bootstrap_key, engine, event_bus, _task_queue) =
    aeordb::server::create_app_with_auth_mode(
        &aeordb_path_str,
        &aeordb::auth::AuthMode::SelfContained,
        None,    // no hot dir
        None,    // no CORS flag (handled upstream by nginx)
    );
```

The returned router (aeordb's full router with its own `AppState`) is nested under a non-public path prefix `/_aeordb/...` inside our own axum app. Our own routes — `/api/health` and `/api/auth/magic-link` — sit alongside it on the outer router and share state via a small struct holding the same `Arc<StorageEngine>` and `Arc<EventBus>`:

```rust
pub struct AppState {
    pub engine:          Arc<StorageEngine>,
    pub event_bus:       Arc<EventBus>,
    pub public_base_url: String,
}

let our_routes = Router::new()
    .route("/api/health",        get(health))
    .route("/api/auth/magic-link", post(routes::magic_link::request))
    .with_state(state.clone());

let app = our_routes.nest("/_aeordb", aeordb_router);
```

We use `AuthMode::SelfContained`. The engine file lives at `/opt/xenocept-www-server/data/xenocept-www.aeordb` and is owned/written by a dedicated `xenocept-www` system user via a systemd unit. No other process opens the file.

We **reuse the engine and event_bus** returned from `create_app_with_auth_mode` in both our own handlers and aeordb's nested handlers — they are the same `Arc<StorageEngine>` underneath.

### Why we have two writes in the same handler

Xenocept uses **passwordless magic-link signup**. The flow is:

- User submits an email at `/account/sign-in/`.
- Server POST handler at `/api/auth/magic-link` is invoked.
- AeorDB's built-in `request_magic_link` (in `aeordb-lib`) **stores a `MagicLinkRecord` but does not call `send_email`** — sending is the application's responsibility. So we wrote our own handler that does the full flow (generate code, store record, look up email config, send email).
- AeorDB's built-in `verify_magic_link` (also in `aeordb-lib`) expects a pre-existing `User` record — it does `get_user_by_username(record.email)` and returns 401 if `None`. So our request handler must also create a user record on the first sign-in for a given email.

That gives us, in the same handler invocation:

- `system_store::store_user` (which internally calls `store_group` to create the per-user auto-group — so effectively **two** writes from this one call: `/.aeordb-system/users/{uuid}` and `/.aeordb-system/groups/user:{uuid}`)
- `system_store::store_magic_link` (one write: `/.aeordb-system/magic-links/{code_hash}`)

Both pass the same `RequestContext` constructed from the same event bus:

```rust
let ctx = aeordb::engine::request_context::RequestContext::with_bus(state.event_bus.clone());
```

---

## The Bug

### Reproduction

In our `POST /api/auth/magic-link` handler, with the following order (the bug):

```rust
// 1. Look up user → not found
match system_store::get_user_by_username(&state.engine, &email)? {
    Some(_) => { /* nothing */ }
    None    => {
        let user = User::new(&email, Some(&email));
        system_store::store_user(&state.engine, &ctx, &user)?;
        //          ^^^^^^^^^^^^ writes user + auto-group
    }
}

// 2. Then store the magic-link record
system_store::store_magic_link(&state.engine, &ctx, &record)?;
```

Then on the next request — `GET /_aeordb/auth/magic-link/verify?code=<plaintext>` handled by aeordb's own `verify_magic_link`:

```rust
let code_hash = hash_magic_link_code(&query.code);
let record = match system_store::get_magic_link(&state.engine, &code_hash)? {
    Some(record) => record,
    None => return 401 "Invalid or expired magic link",
                // ^^^^ HIT EVERY TIME with the buggy order
    ...
};
```

`get_magic_link` returns `Ok(None)` 100% of the time. The hash is identical (SHA-256 of the same plaintext code, verified by hand). The engine `Arc` is the same one used by `store_magic_link`. No process restart between the two calls.

### What we *also* see (likely related)

Server log on every startup:

```
WARN aeordb::engine::storage_engine: Corrupt or missing hot tail — will rebuild KV from WAL (dirty startup)
WARN aeordb::engine::storage_engine: Dirty startup: rebuilding KV index from full WAL scan...
INFO aeordb::engine::storage_engine: Rebuilding KV index from append log...
WARN aeordb::engine::entry_scanner: Corrupt entry header at offset 71138: Invalid magic bytes. Scanning for next valid entry...
WARN aeordb::engine::entry_scanner: No valid entry found after offset 71138. Stopping scan.
INFO aeordb::engine::storage_engine: KV rebuild complete: 26 entries indexed in 0.01s
WARN aeordb::engine::directory_ops: Root directory exists but appears empty. Run 'aeordb verify --repair' if data is missing.
```

These warnings appear on **every** startup against an `.aeordb` file we've been writing to, and they appear immediately after the file is first created too — so they may be benign chatter, but they cluster around the same area we're seeing the lookup failure, so flagging them in case they're related.

### Direct verification that the bytes are on disk

```
=== store file size BEFORE ===
-rw-r--r-- 1 xenocept-www xenocept-www 91235 May 20 13:48 xenocept-www.aeordb

=== POST a magic-link ===  (returns 200)

=== store file size AFTER ===
-rw-r--r-- 1 xenocept-www xenocept-www 94391 May 20 13:48 xenocept-www.aeordb
```

The file grew by 3156 bytes — consistent with the magic-link JSON record + WAL overhead. So the write hit the WAL.

`store_magic_link` returned `Ok(())`. Our handler checks the result and logs on error; no error line in the journal.

### The Fix (purely a reordering)

```rust
// 1. Store magic-link FIRST
system_store::store_magic_link(&state.engine, &ctx, &record)?;

// 2. Then look up / create user
match system_store::get_user_by_username(&state.engine, &email)? {
    Some(_) => { /* nothing */ }
    None    => {
        let user = User::new(&email, Some(&email));
        system_store::store_user(&state.engine, &ctx, &user)?;
    }
}
```

After this reordering, the very next `get_magic_link(code_hash)` finds the record. Verify succeeds, JWT is issued, end-to-end auth works. The reordering is the only change.

---

## What I'd Like the DB Team to Look At

1. **Why does the order of two unrelated `system_store::*::put` calls (different `JsonStore` instances, different path prefixes — `/.aeordb-system/users/`, `/.aeordb-system/groups/`, `/.aeordb-system/magic-links/`) affect whether the second write is readable?**

2. **Is there some KV-index transactional invariant being violated when `store_user` (which internally also calls `store_group`) happens before another `JsonStore::put`?** `store_user` does:
   ```rust
   USER_STORE.put(engine, ctx, &uuid_string, user)?;
   let group_name = format!("user:{}", user.user_id);
   let auto_group = Group::new(&group_name, "crudlify", "........", "user_id", "eq", &user.user_id.to_string())?;
   store_group(engine, ctx, &auto_group)?;   // a second put
   ```
   So calling `store_user` is effectively two `store_file_buffered` calls. Then our own `store_magic_link` is a third. Maybe the issue surfaces specifically after **three** writes in one ctx, not two?

3. **Is the `RequestContext::with_bus` constructor missing something a write needs?** Both aeordb's own `request_magic_link` handler and ours use the same `RequestContext::with_bus(event_bus.clone())` — and aeordb's own handler (with just a single `store_magic_link` write per request) works fine. The difference is purely the *additional* writes we make in the same handler.

4. **Is there an interaction between `store_group`'s special semantics and `JsonStore::put`?** Groups in aeordb have query-DSL fields (`crudlify`, `user_id`, `eq`, etc.) that look like they trigger predicate evaluation when other writes happen. Could a group write be installing a predicate that blocks reads of a later-written entry?

5. **Why are the "Corrupt entry header" / "Root directory exists but appears empty" warnings emitted on every startup**, even on a freshly-created store with no prior corruption?

---

## Repro Project

The exact handler that demonstrates the bug is in our repo at:

```
xenocept-workspace/xenocept-www/server/src/routes/magic_link.rs
```

Commits to look at:
- The handler in its broken state (writes user first) reproduces the issue.
- A `git diff` would show only the move of the magic-link store ahead of the user store fixed it.

### Minimal reproducer outside our codebase

A standalone reproducer would look roughly like:

```rust
use aeordb::auth::magic_link::{generate_magic_link_code, hash_magic_link_code, MagicLinkRecord, DEFAULT_MAGIC_LINK_EXPIRY_SECONDS};
use aeordb::engine::{StorageEngine, request_context::RequestContext, event_bus::EventBus, system_store, user::User};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let engine = Arc::new(StorageEngine::create("/tmp/repro.aeordb")?);
    let event_bus = Arc::new(EventBus::new());
    let ctx = RequestContext::with_bus(event_bus.clone());

    let code = generate_magic_link_code();
    let code_hash = hash_magic_link_code(&code);
    let now = chrono::Utc::now();
    let record = MagicLinkRecord {
        code_hash: code_hash.clone(),
        email: "repro@example.com".to_string(),
        created_at: now,
        expires_at: now + chrono::Duration::seconds(DEFAULT_MAGIC_LINK_EXPIRY_SECONDS),
        is_used: false,
    };

    // Buggy order:
    let user = User::new("repro@example.com", Some("repro@example.com"));
    system_store::store_user(&engine, &ctx, &user)?;       // writes user + auto-group
    system_store::store_magic_link(&engine, &ctx, &record)?;

    // Same process, immediate readback:
    let found = system_store::get_magic_link(&engine, &code_hash)?;
    assert!(found.is_some(), "expected magic-link record to be findable");
    //                       ^^^^^ THIS ASSERTION FAILS

    Ok(())
}
```

If a maintainer wants to run our actual server against the bug, the repro path is:

1. Clone xenocept-www, build `cargo build --release` in `server/`.
2. Revert the magic-link handler's write order to user-first, magic-link-second.
3. Start the server, POST `{"email":"x@y.test"}` to `/api/auth/magic-link`.
4. GET `/_aeordb/auth/magic-link/verify?code=<code-from-log>` — 401 with "Invalid or expired".
5. Swap the order back — works.

---

## Workaround in place

We've reordered our handler to write the magic-link record **first**, then the user record. The user creation is for first-time visitors and doesn't need to happen before the magic-link write — the verify step that needs the user is on the *next* request, by which time both writes have committed.

This is fine for our use case. Filing this report because the silent failure (write returns Ok, file grows, subsequent read says NotFound) is the kind of thing that's hard to diagnose without a lot of detective work, and we'd rather not have other consumers of `aeordb-lib` step on it.

---

## Contact

The Xenocept team — `sales@aeor-development.com`. Happy to attach a copy of a corrupted `.aeordb` file (the one the bug was originally found in) if useful for forensics; let me know.

---

## DB-team investigation (2026-05-20)

**Status:** Could not reproduce in isolation. Need the FS-Server1 backup file to diagnose definitively. Several diagnostic tools added to surface this class of corruption when it does happen.

### What I tried

Four reproduction tests in `aeordb-lib/spec/engine/system_store_spec.rs` — all currently PASS, none reproduce the reported `get_magic_link → None` symptom:

1. `store_user_then_magic_link_then_get_magic_link` — the standalone repro from this report's "Minimal reproducer outside our codebase" snippet, near-verbatim.
2. `handler_flow_email_signup_first_time` — uses `RequestContext::with_bus(event_bus.clone())` (the exact constructor your handler uses) and walks the same get_user → store_user → store_magic_link sequence.
3. `parent_dir_must_accumulate_children_across_writes` — directly tests the hypothesis that successive writes under `/.aeordb-system/*` might be stomping each other's parent-dir children. With a seed of `config` + `cluster` followed by `store_user` + `store_magic_link`, all five children survive.
4. `parent_dir_must_survive_dirty_restart` — sequence of writes, deliberate `drop(engine)` without `shutdown()` to simulate SIGKILL, reopen, more writes, all under the user→magic-link order. After the dirty restart all five `/.aeordb-system` children are still listable.

### What I found in your local-dev DB

Probed `/home/wyatt/Projects/xenocept-workspace/xenocept-www/server/data/xenocept-www.aeordb` (the in-tree copy) with the extended `aeordb probe` tool:

- `/.aeordb-system` content is 255 bytes with children `{config, cluster, magic-links}` (matches the bug report).
- `aeordb probe --path=--list-files` enumerates every FileRecord by path. **There are only three distinct file paths:**
  ```
  /.aeordb-system/cluster/node_id          (8B)
  /.aeordb-system/magic-links/3950dd9e…    (215B)
  /.aeordb-system/config/jwt_signing_key   (32B)
  ```
  **No user records. No group records.** The `store_user` call apparently was never reached in this DB — not "called, succeeded, and lost".

That doesn't contradict your report (your original bug was on a different DB, the `.bak-20260520-134904` on FS-Server1), but it does mean this local-dev copy can't reproduce the lookup-misses-on-disk-data scenario you described.

### Possible explanations consistent with the evidence

- **Your handler returned early before reaching `store_user`** on the requests reflected in this local DB. The current handler's bail-out paths (rate limit, email malformed, magic-link store error, email-config load error, user-lookup error) all `return ok_response()` before user creation. If any of those fired during your testing, the on-disk file would show exactly what we see: the magic-link store completed (file grew, `/.aeordb-system/magic-links` populated) but `/.aeordb-system/users` was never created. This wouldn't explain the FS-Server1 backup's behavior, only this local one.
- **The FS-Server1 backup is genuinely corrupted** in a way the local copy isn't — most likely from earlier soak-test crashes (your startup logs show `Corrupt or missing hot tail` and `Corrupt entry header at offset 71138` on every boot, which means SIGKILL-style shutdowns have been the norm). The combination of corruption at offset 71138 + the scanner giving up + subsequent writes can produce stale-but-live KV pointers (see prior dir_key fix in `1bdda9c`). Without the actual file I can't tell whether the `store_user → store_magic_link` order matters or whether the corruption is upstream of any specific write order.

### Answers to your specific questions

1. **Why does order matter between unrelated `system_store::*::put` calls?** In a clean engine state it doesn't — that's what my four passing tests demonstrate. The order-sensitivity you observed is most likely an interaction with the pre-existing corruption visible in your startup logs, not a write-time ordering bug.

2. **Is `store_user`'s implicit `store_group` violating a KV-index invariant?** No invariant violation found. Both calls go through `JsonStore::put` → `DirectoryOps::store_file_buffered` → `update_parent_directories`, and `update_parent_directories` correctly accumulates child entries (`children.push(current_child_entry)` after a `find(|c| c.name == ...).is_none()`). My `parent_dir_must_accumulate_children_across_writes` test exercises this exact pattern (3 writes under one parent) end-to-end.

3. **Is `RequestContext::with_bus` missing something writes need?** No. `with_bus` and `system()` differ only in the `user_id` field (`"system"` vs the `with_bus` default of `"system"` — they're equivalent). Neither carries any KV-index state.

4. **Does `store_group`'s query-DSL semantics install a predicate that blocks reads?** No. `store_group` is a thin `GROUP_STORE.put` call — a single `JsonStore::put`. The query fields (`crudlify`, `user_id`, `eq`) are static metadata serialized into the JSON payload; they're never evaluated at write or read time. Predicate evaluation happens lazily in `Group::evaluate_membership(user)` when the permission resolver walks group memberships, never during `put`/`get`.

5. **Why are "Corrupt entry header" / "Root directory exists but appears empty" warnings emitted on every startup?** These are **not benign in your case** and ARE the most likely root cause of the data damage. They mean every server shutdown is leaving the WAL in a state the next startup can't fully recover — i.e. real corruption is accreting at a known offset. The dirty-startup path then scans forward, gives up after a 1MB search window finds no valid entry, and treats anything past offset 71138 as orphan space. Two engine-level findings here:
   - **Investigate why the hot tail is dirty on every shutdown.** If `xenocept-www-server` is being SIGTERM'd by systemd and the process is exiting before the hot tail's 100ms flush timer can fire, that's the source. Either (a) install a graceful-shutdown handler in your server that calls `engine.shutdown()` before exit, or (b) tighten the hot-tail flush cadence. The aeordb CLI's `start` command installs a tokio signal handler that calls `engine.shutdown()` on SIGTERM/SIGINT; if your wrapper doesn't replicate that, every restart is effectively a crash.
   - **The 71138-offset corruption is sticky.** Once it's there, every subsequent rebuild stops at the same point. `aeordb verify --repair` rebuilds the KV from a fresh WAL scan and will skip-past unreadable regions, which should let the engine make forward progress.

### Diagnostic tools shipped

Useful on the FS-Server1 backup when you can copy it over:

- `aeordb probe -D <path> --path "/.aeordb-system"` — inspects a single dir's path-key + content hash + parent listing of the corresponding entry. Shows hard-link target liveness so you can tell at a glance whether a dir_key points at a dead/diverged content hash.
- `aeordb probe -D <path> --path=--list-files` — enumerates every FileRecord path in the DB (dedup'd; one line per distinct path with size). Lets you confirm whether `store_user` writes actually made it to disk vs being short-circuited upstream.
- `aeordb probe -D <path> --path=--wal-dump` — prints every DirectoryIndex KV entry with its hash, value length, and hard-link target. Orphan content blobs (length > 0 but not pointed to by any live dir_key) are visible at a glance — they're stale-write artifacts and a strong signal that the dir_key got out of sync with the merkle tree.
- `aeordb verify --repair` already detects and self-heals stale dir_key hard-links (prior bug, commit `1bdda9c`). Run it against the buggy backup as a first step before deeper forensics.

### Asks back to xenocept-www

1. **Send the FS-Server1 backup file** if convenient — that's the only way to definitively reproduce the lookup-miss behavior. I'll run it through the diagnostic suite and through the actual `verify_magic_link` handler under tracing to see exactly where the KV lookup loses the entry.
2. **Check the systemd unit's `KillMode=` / `ExecStop=` / `TimeoutStopSec=`.** If systemd is SIGKILL'ing the process or the process exits without invoking `engine.shutdown()` (e.g. on `panic!` or an `anyhow::Result` early-return from `main`), every shutdown is a crash from the engine's perspective. Install a `ctrl_c` + `signal::unix::SignalKind::terminate` handler in your `run_server()` that calls `engine.shutdown()` before returning. That alone should eliminate the "dirty startup on every boot" pattern.
3. **In your `request` handler, log at INFO when `store_user` actually runs** (not just on error). If the FS-Server1 backup also lacks user records, that confirms the handler is short-circuiting before user creation rather than the writes being lost.

If after the shutdown fix + a fresh DB the bug still reproduces against the canonical order, please re-file with the new corroborating data — I'll prioritize.

— DB team

---

## DB-team follow-up (2026-05-20, root cause found)

After pulling the FS-Server1 backup down and probing it, the read-back failure was **not** a write-order bug. It was a **library-setup bug on our side**.

### What was actually wrong

The `aeordb-cli start` command spawns a 100ms tokio task that calls `engine.try_flush_hot_buffer()` on every tick. This is what makes write-buffered KV entries durable to the hot tail before the 512-entry threshold kicks in (which low-traffic workloads never hit).

`create_app_with_auth_mode(...)` — the entry point every library consumer (including `xenocept-www-server`) uses — **did not spawn that timer**. So for any library consumer:

- A single `store_magic_link` write enters `write_buffer` / `hot_buffer` and sits there in memory.
- `engine.shutdown()` (called from `StorageEngine::Drop`) is the only thing that flushes it to disk.
- Drop only runs on a graceful exit. Your `main.rs` does `axum::serve(listener, app).await?` with no signal handling, so systemd SIGTERMs the process and Drop never runs.
- Next boot: WAL scanner finds nothing past the last clean offset, recovers stale-but-clean state, and the recent magic-link write is gone.

This also explains the `Corrupt entry header at offset N` warnings on every startup — the offset crept up by ~3-8 KB per session in your logs (71138 → 74929 → 77334 → 85043), exactly matching small in-memory writes lost on each SIGTERM.

### Fix shipped (this commit)

- `aeordb::server::spawn_hot_buffer_flush_timer(engine, cancel)` — new exported helper.
- `create_app_with_auth_mode_and_cancel` now calls it during construction, so **every library consumer gets the 100ms flush by default**.
- CLI's hand-rolled copy of the timer block removed.
- Regression smoke test: `timer_flushes_writes_without_explicit_shutdown`.

### What you should still do on your side

The timer narrows the loss window from "all in-memory writes" to "≤100ms of in-memory writes". For full durability under SIGTERM you still want graceful shutdown. Add to `xenocept-www/server/src/main.rs`:

```rust
let cancel = tokio_util::sync::CancellationToken::new();
let (aeordb_router, bootstrap_key, engine, event_bus, _task_queue) =
    aeordb::server::create_app_with_auth_mode_and_cancel(
        &aeordb_path_str,
        &AuthMode::SelfContained,
        None, None,
        Some(cancel.clone()),
    );

// ...build `app` as before...

let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
axum::serve(listener, app)
    .with_graceful_shutdown(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel.cancel();
    })
    .await?;

// `engine` Drop runs here when the function returns and the Arc count hits zero,
// triggering shutdown() which flushes any final pending writes.
engine.shutdown();
```

That eliminates the dirty-startup cycle entirely. With both fixes in place the next backup should show a clean hot tail on every restart and no scanner-stops-at-offset warnings.

### Net to xenocept-www

- **You can revert the magic-link-first workaround.** The original `store_user → store_magic_link` order is correct; both writes are durable inside the same 100ms window once you upgrade `aeordb`.
- **Recommend installing the graceful-shutdown handler above** before the next deploy. The timer is sufficient defense for tests but a graceful shutdown is what you actually want in production.

— DB team
