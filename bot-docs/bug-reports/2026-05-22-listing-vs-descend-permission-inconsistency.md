# Bug Report — Parent listing reports `crudlify` perms; descend into same dir returns 403 Forbidden

**Filed by:** aeordb-client team
**Target:** aeordb (permission evaluation in `/files/list` vs `/files/browse` for the same path, with the same auth context)
**Severity:** High — silently breaks bidirectional sync. Pulls report "0 pulled, 0 failed" while the directory is actually inaccessible; the user gets an empty local directory and no surfaced error.
**Date:** 2026-05-22

---

## TL;DR

The aeordb engine returns mutually contradictory permission decisions for the same `(user, path)` pair depending on which endpoint asks:

- `GET /files/browse/<rel>/Family` lists `Susan` as a child with `effective_permissions: "crudlify"` and `size: 351`.
- `GET /files/browse/<rel>/Family/Susan` for that same user, in the same session, returns **HTTP 403 Forbidden**.

The client's bidirectional sync therefore can't enumerate `Susan` to pull its contents, but never surfaces an error — it reports `0 pulled, 0 skipped, 0 failed` and moves on. The user sees an empty local directory containing only the client's `.aeordb-permissions` stub (which the client wrote claiming `crudlify`, matching the listing the engine handed it).

Two possible root causes, both engine-side:

1. **Listing-side bug:** `effective_permissions` in directory listings is computed from a stale / over-broad inheritance path and reports `crudlify` for entries that, at descend time, evaluate to deny.
2. **Descend-side bug:** the listing is correct (user really does have `crudlify` on Susan), but the descend / enumerate code path uses a different ACL evaluation that incorrectly returns 403.

A grep for where directory listings populate `effective_permissions` vs where `/browse/<path>` evaluates "can this user enumerate" should make it obvious which side disagrees. The two need to share a single permission-evaluation entry point.

---

## Observed state on aeordb-client

User reported "the `Family/Susan/` folder is empty locally even though the DB has files there." The "Open Locally" affordance in the file browser opened the local sync directory in Nautilus — confirming the directory really is empty (1 file: the permissions stub).

### Relationship config

```
id:                   738afafa-39f1-4eb2-b554-8a1fc5eaae7a
name:                 AeorDB Test
remote_path:          /Pictures/
local_path:           /home/wyatt/Documents/AeorDB Tests/Sync Test
direction:            bidirectional
delete_propagation:   { local_to_remote: false, remote_to_local: false }
enabled:              true
```

### Local filesystem state of `Family/Susan/`

```
$ ls -la "/home/wyatt/Documents/AeorDB Tests/Sync Test/Family/Susan/"
total 5
drwxrwxrwx 1 wyatt wyatt    0 May 22 16:00 .
drwxrwxrwx 1 wyatt wyatt 4096 May 22 16:00 ..
-rwxrwxrwx 1 wyatt wyatt  102 May 22 16:00 .aeordb-permissions

$ cat "/home/wyatt/Documents/AeorDB Tests/Sync Test/Family/Susan/.aeordb-permissions"
{"links":[{"group":"user:fcb51064-465a-433c-97be-694b09b0e1bd","allow":"crudlify","deny":"........"}]}
```

For comparison, the sibling `Aeolus/` (which DOES have its expected files locally):

```
$ cat "/home/wyatt/Documents/AeorDB Tests/Sync Test/Family/Aeolus/.aeordb-permissions"
{"links":[{"group":"user:fcb51064-465a-433c-97be-694b09b0e1bd","allow":".r..l...","deny":"........"}]}
```

So the client's local cache thinks the user has FULL access (`crudlify`) to Susan, but only READ + LIST on Aeolus. Yet Aeolus syncs cleanly and Susan does not.

### Engine response 1 — parent listing

`GET /api/v1/browse/738afafa.../Family` via the aeordb-client proxy to the remote:

```json
{
  "remote_path": "/Pictures/Family",
  "entries": [
    { "name": "Aeolus", "entry_type": 3, "size": 15218,
      "effective_permissions": "-r--l---" },
    { "name": "Harlo",  "entry_type": 3, "size": 3091,
      "effective_permissions": "-r--l---" },
    { "name": "Susan",  "entry_type": 3, "size": 351,
      "effective_permissions": "crudlify" }
  ],
  "total": 3
}
```

The engine confidently reports: `Susan` has 351 bytes of content AND the user has `crudlify` permissions on it.

### Engine response 2 — descend into the same directory

`GET /api/v1/browse/738afafa.../Family%2FSusan` (same user, same auth, same client process, immediately after the listing above):

```json
{
  "error": "bad gateway: server error: remote returned HTTP 403 Forbidden for /Pictures/Family/Susan"
}
```

Same path. Same user. Same session. Different answer.

### Sync activity (12+ consecutive runs since Susan appeared)

```
pull sync complete for 'AeorDB Test': 0 pulled, 0 skipped, 0 failed, 0 deleted, 0 symlinks
pull sync complete for 'AeorDB Test': 0 pulled, 0 skipped, 0 failed, 0 deleted, 0 symlinks
pull sync complete for 'AeorDB Test': 0 pulled, 0 skipped, 0 failed, 0 deleted, 0 symlinks
... (10+ more, all identical)
```

No errors. No 403s logged. No "failed to enumerate" warnings. The sync just silently does nothing on every cycle.

### Timestamps

- Remote `Susan` `created_at`: `1779490808209` → 2026-05-22 16:00:08 UTC (today)
- Local directory `Susan/` mtime: 2026-05-22 16:00:51 (43 seconds after remote creation)

So the directory was created on the remote first, then the client picked it up and created the local stub. Whatever wrote the local `.aeordb-permissions: "crudlify"` got that value from somewhere — most likely the same parent-listing response that the engine still returns today.

---

## The contradiction

Restating because it's the whole bug:

| Endpoint (same user, same path) | What the engine says |
|---|---|
| List `/Pictures/Family` → look at `Susan` child | `effective_permissions: "crudlify"`, `size: 351` |
| Browse `/Pictures/Family/Susan` directly | `403 Forbidden` |

Either:

- **(A) Listing-side bug.** The `effective_permissions` value in directory listings is computed from a different (broader / stale / inherited-incorrectly) ACL than the one enforced at descend time. Users see "crudlify" but actually can't do anything.
- **(B) Descend-side bug.** The listing's `crudlify` is correct, but the per-directory enumeration check uses a stricter / wrong path and 403s the request.

We can't tell from outside which side is wrong without instrumented logging on the engine. Whichever it is, the two evaluations need to converge through a single permission-evaluation function — having two divergent code paths for "can this user see / enumerate this directory" is what produced the inconsistency in the first place.

---

## Why this is high severity for clients

The client's bidirectional sync trusts the parent listing's `effective_permissions`. When the listing says `crudlify`, the client:

1. Creates the local directory.
2. Writes a `.aeordb-permissions` stub copying `crudlify` from the listing.
3. Calls the engine to enumerate the directory's children for pull.
4. Receives a 403 — and (separately, see below) silently swallows it.

The user is now in a state where:

- They see the directory in the local filesystem.
- They see the directory in the in-app file browser (because the parent listing shows it).
- The directory has the permissions stub the engine asked them to write.
- The directory is empty and won't ever populate.
- The sync activity feed shows green "0 pulled, 0 failed" forever.

There is no surfaced symptom for the end-user other than "the folder is empty." For pull-only or bidirectional relationships, this looks identical to "the folder was always empty" — silently corrupt sync state.

---

## Repro suggestion

1. Create a directory on the engine (say `/Pictures/Family/Susan/`) and grant a user `crudlify` on its **parent**, but a more restrictive ACL on the directory itself such that a direct browse of the directory denies but the parent listing's inheritance computation still resolves to `crudlify`. (Exact recipe depends on aeordb-lib's permission semantics — this report is filed by the client team without engine internals.)
2. As that user, `GET /files/browse/<parent>` and observe `effective_permissions: "crudlify"` for the target child.
3. `GET /files/browse/<parent>/<target>` and observe 403.

If we can't construct this state from the engine's public API, the bug may instead be triggered by some specific race between directory creation, ACL inheritance, and the user record — in which case the engine team needs to instrument the two endpoints and bisect.

---

## Recommended engine-side investigation

1. Find the two functions that produce permission decisions for these endpoints:
   - The one that fills `effective_permissions` in `/files/list` / directory listings.
   - The one that gates `/files/browse/<path>` for descent.
2. Confirm they currently use different ACL-evaluation code paths (they almost certainly do, given the observed contradiction).
3. Replace both with calls into a single `evaluate_permission(user, path) -> Permissions` function. The listing endpoint can format the returned `Permissions` into the wire string; the browse endpoint can check the relevant bits before responding.
4. Add a regression spec that asserts: for any `(user, path)`, the `effective_permissions` exposed in the parent listing's entry MUST match the actual access decision when the path is browsed directly. This is a property test — generate ACL configurations, pick a `(user, path)`, assert equivalence.

---

## Adjacent client-side issue we'll file separately

Independent of the engine bug above, the **client's pull sync silently treats 403-on-enumerate as "nothing to do"** instead of as an error. Even after the engine bug is fixed, any future permission misconfiguration would cause the same "0 pulled, 0 failed" non-symptom. The client team will file a separate report against `aeordb-client-lib/src/sync/pull.rs` to:

- Distinguish "no entries returned" from "couldn't enumerate".
- Surface the 403 to the activity log AND a user-visible toast (`window.aeorToast(...)`).
- Count it in `failed` so the dashboard's per-relationship status reflects the broken state.

Mentioning here so the engine team knows about the eventual reporter-side fix, but it doesn't block the engine investigation.

---

## Footnote — adjacent SSE 401

When the client first connected to this connection today, the SSE listener logged once:

```
WARN aeordb_client_lib::sync::sse_listener:
  SSE connection to 'AeorDB Test' failed: server error: SSE returned HTTP 401 Unauthorized. Retrying in 1s
```

It hasn't recurred — the retry presumably succeeded. Different surface from the `/browse` 403, but flagging it in case the engine team finds a common auth-context drift between SSE and proxied browse. If they're unrelated, ignore.

---

## Contact

aeordb-client running locally at `127.0.0.1:9400`. Relationship id `738afafa-39f1-4eb2-b554-8a1fc5eaae7a` against connection `9a393fc7-7863-4241-8393-21025597bd7b`. Local sync root `/home/wyatt/Documents/AeorDB Tests/Sync Test`. Engine team can reach back through the usual channel; we have the running process available for live probing if you need additional traces.

---

## DB-team resolution (2026-05-22)

**Status:** Fixed. Listing-side was right; descend-side had a trailing-slash mismatch.

### Root cause

`permission_middleware.rs:306` called `PermissionResolver::check_direct_permission(user, path, op)`. That resolver walks `path_levels(path)` — and `path_levels("/A/B/C")` (no trailing slash) returns `["/", "/A", "/A/B"]`, **deliberately omitting the path itself** because it might be a file. A direct grant stored at `/A/B/C/.aeordb-permissions` (which is what a "directly shared this directory" config looks like on disk) is therefore invisible to the middleware unless the caller passes a trailing slash.

The listing path already knew this — `engine_routes.rs:677` explicitly appends `/` to directory entries before calling the resolver, which is why the parent listing correctly reported `crudlify` on Susan. The middleware had no such normalization.

So the two endpoints didn't share a single permission-evaluation function (your hypothesis 1 in the report). They both called into `PermissionResolver`, but the middleware called `check_direct_permission` (raw, no trailing-slash retry) and the listing called the resolver after pre-normalizing the path.

A helper `check_path_permission` already existed in the resolver — documented as "tolerant of the trailing-slash convention: a path like /A/B is treated as a possible directory AND a possible file." It tries the as-given path first, then the directory form, and returns true if either grants. The middleware just wasn't using it.

### Fix shipped

Two minimal changes:

- `aeordb-lib/src/auth/permission_middleware.rs:306` — `check_direct_permission` → `check_path_permission`. The path-without-trailing-slash now also tries the directory form, picking up direct grants stored at the path itself.
- `aeordb-lib/src/server/sync_routes.rs:307` — same change for the SSE sync filter, which had the same anti-pattern. Symlinks-to-directories were the realistic failure mode there.

### Regression test

`aeordb-lib/spec/engine/sharing_spec.rs::direct_share_on_directory_grants_descend_without_trailing_slash` builds the exact scenario:

1. Create `/Pictures/Family/Susan/photo.jpg` as root.
2. Direct grant to a fresh user: `share_directory_with_user("/Pictures/Family/Susan", &user, "crudlify", None)`. No parent grants anywhere.
3. As that user, `GET /files/Pictures/Family/Susan/` → 200 ✓ (was always passing).
4. As that user, `GET /files/Pictures/Family/Susan` → **was 403, now 200**.

Test passes with the fix; would fail without it.

### Verified against the running test DB

Reproduced your scenario as user `fcb51064` (wyatt) on the local test database:

```
Before fix:
  GET /files/Pictures/Family/Susan/  → 200  (3 items, crudlify each)
  GET /files/Pictures/Family/Susan   → 403  Permission denied

After fix (same JWT, same DB):
  GET /files/Pictures/Family/Susan/  → 200
  GET /files/Pictures/Family/Susan   → 200
```

### Bonus: stronger invariant test we should add (followup)

You suggested a property test asserting `listing_eff_perms == descend_decision` for any (user, path). I didn't add that this pass — the current setup-share / user-tokenization helpers in `sharing_spec.rs` would need a generator for random ACL configurations, and that's bigger than the bug. Logging it as a followup so we don't forget; the unit test above pins the specific failure mode in the meantime.

### Net to aeordb-client

- Once you pull the new binary, the 403 on `/Pictures/Family/Susan` (without trailing slash) goes away. Your pull sync should enumerate Susan and pull the 3 files.
- You don't need to change the URL shape on your side — both `/path` and `/path/` will work for shared directories from now on.
- The client-side issue you noted (silently swallowing 403-on-enumerate) is still worth fixing on your end: any future permission misconfiguration would otherwise produce the same "0 pulled, 0 failed" non-symptom. That's your bug to file separately as planned.
- The SSE 401 footnote is unrelated to this — likely a one-shot startup race between SSE listener init and the JWT being ready. If it recurs, file separately and we'll investigate.

— DB team
