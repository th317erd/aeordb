# File Sharing Phase 2 — Link Sharing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable shareable URLs backed by scoped API keys with no user, where the key's crudlify rules are the sole permission authority.

**Architecture:** Three layers: (1) make `ApiKeyRecord.user_id` optional and update auth/permission middleware to handle user-less keys, (2) add share-link CRUD endpoints that create scoped keys + JWTs, (3) activate the Link tab in the Share modal and add portal `?token=` detection.

**Tech Stack:** Rust (axum, serde, jsonwebtoken), JavaScript (web components), existing API key + permission infrastructure

**Spec:** `docs/superpowers/specs/2026-04-25-file-sharing-phase2-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `aeordb-lib/src/auth/api_key.rs` | Modify | Change `user_id: Uuid` → `Option<Uuid>` |
| `aeordb-lib/src/engine/system_store.rs` | Modify | Handle `None` user_id in store/list key functions |
| `aeordb-lib/src/auth/middleware.rs` | Modify | Accept `?token=` query param as auth alternative |
| `aeordb-lib/src/auth/permission_middleware.rs` | Modify | Share keys: rules are grants, skip permission resolver |
| `aeordb-lib/src/server/share_link_routes.rs` | Create | `POST/GET/DELETE /files/share-link(s)` endpoints |
| `aeordb-lib/src/server/mod.rs` | Modify | Register share-link routes |
| `aeordb-lib/src/server/api_key_self_service_routes.rs` | Modify | Handle `Option<Uuid>` in key listing response |
| `aeordb-web-components/components/aeor-file-browser-base.js` | Modify | Activate Link tab with create/copy/revoke UI |
| `aeordb-web-components/components/aeor-file-browser-portal.js` | Modify | Implement share-link API methods |
| `aeordb-lib/src/portal/app.mjs` | Modify | Detect `?token=` param, skip login, use for API calls |
| `aeordb-lib/spec/engine/share_link_spec.rs` | Create | Integration tests |

---

### Task 1: Make ApiKeyRecord.user_id Optional

**Files:**
- Modify: `aeordb-lib/src/auth/api_key.rs`
- Modify: `aeordb-lib/src/engine/system_store.rs`
- Modify: `aeordb-lib/src/server/api_key_self_service_routes.rs`

- [ ] **Step 1: Change `user_id` to `Option<Uuid>` in `ApiKeyRecord`**

In `aeordb-lib/src/auth/api_key.rs`, change:
```rust
pub user_id: Uuid,
```
to:
```rust
pub user_id: Option<Uuid>,
```

- [ ] **Step 2: Fix `store_api_key` in `system_store.rs`**

The function calls `validate_user_id(&record.user_id)`. Change to only validate when `user_id` is `Some`:
```rust
pub fn store_api_key(
    engine: &StorageEngine,
    ctx: &RequestContext,
    record: &ApiKeyRecord,
) -> EngineResult<()> {
    if let Some(ref uid) = record.user_id {
        validate_user_id(uid)?;
    }
    store_api_key_unchecked(engine, ctx, record)
}
```

- [ ] **Step 3: Fix all compilation errors from the type change**

Search for all code that accesses `record.user_id` or constructs `ApiKeyRecord` and update to handle `Option<Uuid>`. Key locations:
- `aeordb-lib/src/server/api_key_self_service_routes.rs` — `create_own_key` sets `user_id: target_user_id` → change to `user_id: Some(target_user_id)`
- `aeordb-lib/src/server/api_key_self_service_routes.rs` — `list_own_keys` filters by `record.user_id == caller_id` → change to `record.user_id == Some(caller_id)`
- `aeordb-lib/src/server/routes.rs` — admin key listing, update serialization
- `aeordb-lib/src/auth/provider.rs` or wherever token exchange maps `record.user_id` to JWT `sub` → when `None`, use `format!("share:{}", record.key_id)` as `sub`
- `aeordb-lib/src/engine/engine_event.rs` — `ApiKeyEventData.target_user_id` may need to handle None
- Any test files that construct `ApiKeyRecord`

Run: `cargo build -p aeordb 2>&1 | head -50` and fix each error.

- [ ] **Step 4: Update token exchange to handle share keys**

In the token exchange handler (where API key → JWT), when `record.user_id` is `None`:
- Set JWT `sub` to `format!("share:{}", record.key_id)`
- Set JWT `key_id` to `Some(record.key_id.to_string())`
- For expiration: if `record.expires_at` is far-future sentinel (see Step 5), set JWT `exp` to the same far-future value

Find the token exchange handler — search for `POST /auth/token` handler or `fn exchange` or `fn token`.

- [ ] **Step 5: Define a sentinel for "no expiry" share keys**

In `aeordb-lib/src/auth/api_key.rs`, add:
```rust
/// Sentinel value for "never expires" keys. Year 2200 in milliseconds.
/// JWT validation requires an `exp` claim, so we use a far-future date
/// rather than omitting it.
pub const NO_EXPIRY_SENTINEL: i64 = 7_258_118_400_000; // 2200-01-01T00:00:00Z
```

- [ ] **Step 6: Verify compilation and run existing tests**

Run: `cargo build -p aeordb 2>&1 | tail -5`
Run: `cargo test --test api_key_rules_spec --test api_key_cache_spec --test permissions_spec 2>&1 | tail -10`

- [ ] **Step 7: Commit**

```bash
git add aeordb-lib/src/auth/api_key.rs aeordb-lib/src/engine/system_store.rs aeordb-lib/src/server/api_key_self_service_routes.rs aeordb-lib/src/server/routes.rs aeordb-lib/src/auth/
git commit -m "Make ApiKeyRecord.user_id optional for share keys"
```

---

### Task 2: Auth Middleware — Accept `?token=` Query Param

**Files:**
- Modify: `aeordb-lib/src/auth/middleware.rs`

- [ ] **Step 1: Update auth middleware to check query params**

In `aeordb-lib/src/auth/middleware.rs`, the current code extracts the token only from the `Authorization: Bearer` header. Add a fallback to `?token=` query param:

Replace the token extraction block (lines ~40-63) with:
```rust
  // Extract token from Authorization header or ?token= query param
  let authorization_header = request
    .headers()
    .get("authorization")
    .and_then(|value| value.to_str().ok())
    .map(|value| value.to_string());

  let token_from_header = authorization_header
    .as_ref()
    .filter(|h| h.starts_with("Bearer "))
    .map(|h| h[7..].to_string());

  let token_from_query = if token_from_header.is_none() {
    request.uri().query()
      .and_then(|q| {
        q.split('&')
          .find(|pair| pair.starts_with("token="))
          .map(|pair| pair[6..].to_string())
      })
  } else {
    None
  };

  let token = match token_from_header.or(token_from_query) {
    Some(t) => t,
    None => {
      tracing::warn!("Auth failed: missing or invalid Authorization header");
      metrics::counter!(
        crate::metrics::definitions::AUTH_VALIDATIONS_TOTAL,
        "result" => "missing_header"
      ).increment(1);
      return (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse {
          error: "Missing or invalid Authorization header".to_string(),
          code: None,
        }),
      )
        .into_response();
    }
  };
```

Then change the `verify_token` call to use `&token` (owned String) instead of the previous `token` slice.

- [ ] **Step 2: Verify compilation**

Run: `cargo build -p aeordb 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/src/auth/middleware.rs
git commit -m "Auth middleware: accept ?token= query param as Bearer alternative"
```

---

### Task 3: Permission Middleware — Handle Share Keys

**Files:**
- Modify: `aeordb-lib/src/auth/permission_middleware.rs`

- [ ] **Step 1: Detect share keys and skip permission resolver**

In `aeordb-lib/src/auth/permission_middleware.rs`, the middleware currently:
1. Parses `user_id` from `claims.sub` (line 82) — fails if not a UUID
2. Checks API key rules (lines 106-190) — restricts access
3. Runs permission resolver (lines 192-217) — checks user groups

For share keys (`sub` starts with `"share:"`), we need to:
1. Detect the share key pattern
2. Let key rules pass (they already work — the grant vs restrict distinction is handled by the fact that there's no further permission check)
3. Skip the permission resolver entirely

Change the `user_id` parsing block (lines 80-98) to:
```rust
  // Parse user_id from claims.sub.
  // Share keys have sub = "share:{key_id}" — they bypass the permission resolver
  // and use key rules as the sole permission authority.
  let is_share_key = claims.sub.starts_with("share:");
  let user_id = if is_share_key {
    None
  } else {
    match Uuid::parse_str(&claims.sub) {
      Ok(user_id) => Some(user_id),
      Err(_) => {
        tracing::warn!(sub = %claims.sub, "Rejecting request: sub is not a valid UUID");
        return (
          StatusCode::FORBIDDEN,
          Json(ErrorResponse {
            error: "Invalid user identity".to_string(),
            code: None,
          }),
        )
          .into_response();
      }
    }
  };
```

Then update the permission resolver section (lines 192-217) to skip when `is_share_key`:
```rust
  // For share keys, the API key rules are the sole permission authority.
  // Skip the user/group permission resolver entirely.
  if is_share_key {
    return next.run(request).await;
  }

  // Check permission (normal user flow).
  let resolver = PermissionResolver::new(
    &state.engine,
    &state.group_cache,
    &state.permissions_cache,
  );

  match resolver.check_permission(&user_id.unwrap(), engine_path, operation) {
```

Note: The existing API key rules enforcement block (lines 106-190) already works correctly for share keys — it checks `claims.key_id`, loads the key record, validates revocation/expiration, and enforces rules. The only difference for share keys is that we DON'T run the permission resolver after.

- [ ] **Step 2: Verify compilation**

Run: `cargo build -p aeordb 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/src/auth/permission_middleware.rs
git commit -m "Permission middleware: share keys bypass resolver, use rules as grants"
```

---

### Task 4: Share Link Endpoints

**Files:**
- Create: `aeordb-lib/src/server/share_link_routes.rs`
- Modify: `aeordb-lib/src/server/mod.rs`

- [ ] **Step 1: Create share_link_routes.rs**

Read these files first for patterns:
- `aeordb-lib/src/server/share_routes.rs` — existing share endpoint patterns
- `aeordb-lib/src/server/api_key_self_service_routes.rs` — API key creation pattern
- `aeordb-lib/src/auth/api_key.rs` — `generate_api_key`, `hash_api_key`, `ApiKeyRecord`, `NO_EXPIRY_SENTINEL`
- `aeordb-lib/src/engine/api_key_rules.rs` — `KeyRule`, `parse_rules_from_json`

Create `aeordb-lib/src/server/share_link_routes.rs` with three handlers:

**`POST /files/share-link`** — creates a scoped API key (user_id: None) + JWT:
```rust
use axum::{Extension, extract::State, http::StatusCode, response::{IntoResponse, Response}, Json};
use serde::Deserialize;
use uuid::Uuid;

use super::responses::ErrorResponse;
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::auth::api_key::{generate_api_key, hash_api_key, ApiKeyRecord, NO_EXPIRY_SENTINEL};
use crate::auth::jwt::TokenClaims as JwtClaims;
use crate::engine::api_key_rules::KeyRule;
use crate::engine::user::is_root;

#[derive(Deserialize)]
pub struct CreateShareLinkRequest {
    pub paths: Vec<String>,
    pub permissions: String,
    pub expires_in_days: Option<i64>,
    pub base_url: Option<String>,
}
```

Implementation:
1. Validate caller is root
2. Validate permissions string is 8 chars
3. Build rules: for each path, create `KeyRule { path_pattern: "{path}**", permitted: permissions }`, plus fallback `KeyRule { path_pattern: "**", permitted: "........" }`
4. Calculate expiration: if `expires_in_days` is None, use `NO_EXPIRY_SENTINEL`; otherwise compute from days
5. Create `ApiKeyRecord` with `user_id: None`, generated label like `"Share: {first_path} ({perm_label})"`, rules, expiration
6. Store via `state.auth_provider.store_api_key(&record)` (the updated store_api_key handles None user_id)
7. Build JWT claims: `sub: "share:{key_id}"`, `key_id: Some(key_id)`, `exp` matching the key expiration
8. Create token via `state.jwt_manager.create_token(&claims)`
9. Build URL: `{base_url}/system/portal/?token={jwt}&path={first_path}` (base_url from request body, or derive from `Host` header)
10. Return `{ url, token, key_id, permissions, expires_at, paths }`

Helper for permission label:
```rust
fn permission_label(perms: &str) -> &str {
    match perms {
        "cr..l..." => "View only",
        "crudl..." => "Can edit",
        "crudlify" => "Full access",
        _ => "Custom",
    }
}
```

**`GET /files/share-links?path=...`** — list active share links for a path:
1. List all API keys via system_store
2. Filter to keys where `user_id` is `None` (share keys)
3. Filter to keys whose rules reference the queried path
4. Return `{ path, links: [{ key_id, label, permissions, expires_at, created_at }] }`

**`DELETE /files/share-links/{key_id}`** — revoke a share link:
1. Validate caller is root
2. Delete/revoke the API key by key_id
3. Invalidate API key cache
4. Return `{ revoked: true, key_id }`

- [ ] **Step 2: Register routes in mod.rs**

In `aeordb-lib/src/server/mod.rs`, add `pub mod share_link_routes;` and register:
```rust
    .route("/files/share-link", post(share_link_routes::create_share_link))
    .route("/files/share-links", get(share_link_routes::list_share_links))
    .route("/files/share-links/{key_id}", delete(share_link_routes::revoke_share_link))
```

Register before the `/files/{*path}` wildcard.

- [ ] **Step 3: Verify compilation**

Run: `cargo build -p aeordb 2>&1 | tail -10`

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/server/share_link_routes.rs aeordb-lib/src/server/mod.rs
git commit -m "Add share-link endpoints: create, list, revoke"
```

---

### Task 5: Link Tab UI in Share Modal

**Files:**
- Modify: `aeordb-web-components/components/aeor-file-browser-base.js`
- Modify: `aeordb-web-components/components/aeor-file-browser-portal.js`

- [ ] **Step 1: Add abstract share-link methods to base class**

In `aeor-file-browser-base.js`, add to the abstract methods section:
```javascript
  async createShareLink(paths, permissions, expiresInDays) {
    throw new Error('AeorFileBrowserBase.createShareLink() must be implemented by subclass');
  }

  async getShareLinks(path) { return { links: [] }; }

  async revokeShareLink(keyId) {
    throw new Error('AeorFileBrowserBase.revokeShareLink() must be implemented by subclass');
  }
```

- [ ] **Step 2: Activate the Link tab in `_showShareModal`**

Find the Link tab button (currently disabled with `"Phase 2"` text). Remove the `disabled` attribute and the `style="opacity:0.5;cursor:not-allowed;"`. Change the text from `"Link (Phase 2)"` to just `"Link"`.

Add tab switching logic — when the Link tab is clicked, hide the People content and show the Link content:
```javascript
    // Tab switching
    body.querySelectorAll('.share-tab-btn').forEach((btn) => {
      btn.addEventListener('click', () => {
        if (btn.disabled) return;
        body.querySelectorAll('.share-tab-btn').forEach((b) => b.classList.remove('primary'));
        btn.classList.add('primary');
        const tab = btn.dataset.shareTab;
        const peopleContent = body.querySelector('.share-tab-people');
        const linkContent = body.querySelector('.share-tab-link');
        if (peopleContent) peopleContent.style.display = tab === 'people' ? '' : 'none';
        if (linkContent) linkContent.style.display = tab === 'link' ? '' : 'none';
      });
    });
```

- [ ] **Step 3: Build the Link tab content**

Replace the Phase 2 placeholder with:
```javascript
      <div class="share-tab-link" style="display:none;">
        <div style="margin-bottom:12px;">
          <label style="${labelStyle}">Permission Level</label>
          <select class="link-permission-select" style="${inputStyle}">
            <option value="cr..l...">View only</option>
            <option value="crudl..." selected>Can edit</option>
            <option value="crudlify">Full access</option>
          </select>
        </div>
        <div style="margin-bottom:12px;">
          <label style="${labelStyle}">Expiration</label>
          <select class="link-expiry-select" style="${inputStyle}">
            <option value="">Never</option>
            <option value="1">1 day</option>
            <option value="7">7 days</option>
            <option value="30">30 days</option>
            <option value="90">90 days</option>
            <option value="365">1 year</option>
          </select>
        </div>
        <div style="display:flex;gap:10px;justify-content:flex-end;margin-bottom:16px;">
          <button class="primary small link-create-btn">Create Link</button>
        </div>
        <div class="link-result" style="display:none;margin-bottom:16px;">
          <label style="${labelStyle}">Share URL</label>
          <div style="display:flex;gap:8px;">
            <input type="text" class="link-url-input" readonly style="${inputStyle} flex:1;">
            <button class="secondary small link-copy-btn">Copy</button>
          </div>
        </div>
        <div class="link-active-links">
          <div style="${labelStyle}">Active Links</div>
          ${linkSharesHtml}
        </div>
      </div>
```

Build `linkSharesHtml` from `this.getShareLinks(paths[0])` — fetch active links for the path and render each with a revoke button, similar to the People tab's current shares section.

- [ ] **Step 4: Wire up Link tab event handlers**

Create Link button:
```javascript
    const linkCreateBtn = body.querySelector('.link-create-btn');
    if (linkCreateBtn) {
      linkCreateBtn.addEventListener('click', async () => {
        const permLevel = body.querySelector('.link-permission-select').value;
        const expiryDays = body.querySelector('.link-expiry-select').value;
        const expires = expiryDays ? parseInt(expiryDays) : null;
        try {
          const result = await this.createShareLink(paths, permLevel, expires);
          const resultDiv = body.querySelector('.link-result');
          const urlInput = body.querySelector('.link-url-input');
          resultDiv.style.display = '';
          urlInput.value = result.url;
          if (window.aeorToast) window.aeorToast('Share link created', 'success');
        } catch (error) {
          if (window.aeorToast) window.aeorToast('Failed: ' + error.message, 'error');
        }
      });
    }
```

Copy button:
```javascript
    const linkCopyBtn = body.querySelector('.link-copy-btn');
    if (linkCopyBtn) {
      linkCopyBtn.addEventListener('click', () => {
        const urlInput = body.querySelector('.link-url-input');
        navigator.clipboard.writeText(urlInput.value);
        if (window.aeorToast) window.aeorToast('Copied to clipboard', 'success');
      });
    }
```

Revoke buttons (same pattern as People tab revoke).

- [ ] **Step 5: Implement share-link methods in portal subclass**

In `aeor-file-browser-portal.js`:
```javascript
  async createShareLink(paths, permissions, expiresInDays) {
    const response = await window.api('/files/share-link', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        paths,
        permissions,
        expires_in_days: expiresInDays,
        base_url: window.location.origin,
      }),
    });
    if (!response.ok) throw new Error(`${response.status}`);
    return response.json();
  }

  async getShareLinks(path) {
    const response = await window.api(`/files/share-links?path=${encodeURIComponent(path)}`);
    if (!response.ok) return { links: [] };
    return response.json();
  }

  async revokeShareLink(keyId) {
    const response = await window.api(`/files/share-links/${encodeURIComponent(keyId)}`, {
      method: 'DELETE',
    });
    if (!response.ok) throw new Error(`${response.status}`);
  }
```

- [ ] **Step 6: Verify syntax**

Run: `node -c aeordb-web-components/components/aeor-file-browser-base.js`
Run: `node -c aeordb-web-components/components/aeor-file-browser-portal.js`

- [ ] **Step 7: Commit**

```bash
cd aeordb-web-components && git add components/ && git commit -m "Activate Link tab: create/copy/revoke share links"
```

---

### Task 6: Portal Token Detection

**Files:**
- Modify: `aeordb-lib/src/portal/app.mjs`

- [ ] **Step 1: Detect `?token=` on page load**

In `aeordb-lib/src/portal/app.mjs`, at the top of the initialization code (after AUTH is defined), add token detection:

```javascript
// Detect ?token= query param for share link access
(function detectShareToken() {
  const params = new URLSearchParams(window.location.search);
  const token = params.get('token');
  if (token) {
    AUTH.setToken(token);
    // Mark as share session — don't show full nav
    AUTH._isShareSession = true;
  }
})();
```

- [ ] **Step 2: Navigate to shared path on load**

In the `navigate()` function, after the logged-in pages are shown, check for the `?path=` param and navigate the file browser:

```javascript
  // If this is a share link with ?path=, navigate file browser to that path
  if (AUTH._isShareSession) {
    const params = new URLSearchParams(window.location.search);
    const sharedPath = params.get('path');
    if (sharedPath && activeTag === 'aeor-files') {
      const fb = document.querySelector('aeor-file-browser-portal');
      if (fb && typeof fb.navigateTo === 'function') {
        fb.navigateTo(sharedPath);
      }
    }
  }
```

Check if the file browser component has a `navigateTo` method. If not, the implementing agent should add one (simply sets the active tab's path and calls `browse()`).

- [ ] **Step 3: In share sessions, default to Files page and hide non-applicable nav**

When `AUTH._isShareSession` is true:
- Default the page to `files` instead of `dashboard`
- Optionally hide sidebar items that don't apply (Dashboard, Users, Groups, Keys, Settings) — the share key's scoped rules will return 403/404 for those anyway, but hiding them is cleaner UX

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/portal/app.mjs
git commit -m "Portal: detect ?token= share links, navigate to shared path"
```

---

### Task 7: Integration Tests

**Files:**
- Create: `aeordb-lib/spec/engine/share_link_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Create share_link_spec.rs**

Use the same test patterns as `sharing_spec.rs` and `permissions_spec.rs`. Tests to write:

1. **create_share_link_returns_url_and_token** — POST /files/share-link with a path, verify response has `url`, `token`, `key_id`, `permissions`, `paths`

2. **share_link_token_grants_file_access** — Create a share link for `/photos/`, use the returned token to GET `/files/photos/` — should succeed (200)

3. **share_link_token_denied_outside_scope** — Create a share link for `/photos/`, use token to GET `/files/docs/` — should fail (404, not 403)

4. **share_link_respects_permission_level** — Create a share link with `cr..l...` (read only), use token to PUT a file — should fail

5. **share_link_with_expiry** — Create with `expires_in_days: 1`, verify `expires_at` is set in response

6. **share_link_no_expiry** — Create with `expires_in_days: null`, verify `expires_at` is the far-future sentinel

7. **list_share_links** — Create two share links for different paths, list for one path, verify only the matching one appears

8. **revoke_share_link** — Create a share link, revoke it, use the token — should fail (401)

9. **share_link_requires_root** — Non-root tries to create a share link — should fail (403)

10. **token_in_query_param_works** — Use the token as `?token=JWT` instead of Authorization header — should work

11. **share_key_skips_permission_resolver** — Create share link, verify user does NOT need `.permissions` file to access shared path (rules are the authority)

For the HTTP tests, use the existing patterns: create engine, build app with `create_app_with_jwt`, make requests with `tower::ServiceExt::oneshot`.

For tests that use `?token=`, build the request URI with the token in the query string.

- [ ] **Step 2: Register test in Cargo.toml**

```toml
[[test]]
name = "share_link_spec"
path = "spec/engine/share_link_spec.rs"
```

- [ ] **Step 3: Run tests**

Run: `cargo test --test share_link_spec 2>&1 | tail -20`
Run: `cargo test 2>&1 | grep "FAILED" || echo "ALL TESTS PASS"`

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/spec/engine/share_link_spec.rs aeordb-lib/Cargo.toml
git commit -m "Add share link tests: creation, scoping, auth, revocation, query param"
```

---

### Task 8: Full Verification

- [ ] **Step 1: Run the complete test suite**

Run: `cargo test 2>&1 | grep "test result:" | awk '{sum += $4} END {print "Total:", sum, "tests"}'`

- [ ] **Step 2: Manual E2E test**

```bash
# Start server
target/debug/aeordb start -D /tmp/claude/share-link-test.aeordb --auth self-contained -p 6833 &
sleep 2

# Get the root key from stdout, exchange for token
TOKEN=$(curl -s -X POST http://localhost:6833/auth/token -H "Content-Type: application/json" -d "{\"api_key\": \"$ROOT_KEY\"}" | python3 -c "import sys,json; print(json.load(sys.stdin)['token'])")

# Upload a file
curl -s -X PUT http://localhost:6833/files/photos/sunset.jpg -H "Authorization: Bearer $TOKEN" -d "jpeg-data"

# Create a share link
SHARE=$(curl -s -X POST http://localhost:6833/files/share-link -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" -d '{"paths":["/photos/"],"permissions":"cr..l...","base_url":"http://localhost:6833"}')
echo "$SHARE" | python3 -m json.tool

# Extract the share token and use it
SHARE_TOKEN=$(echo "$SHARE" | python3 -c "import sys,json; print(json.load(sys.stdin)['token'])")
curl -s "http://localhost:6833/files/photos/?token=$SHARE_TOKEN" | python3 -m json.tool

# Verify access outside scope is denied
curl -s -w "\nHTTP %{http_code}\n" "http://localhost:6833/files/docs/?token=$SHARE_TOKEN"

pkill -f "6833"
```

- [ ] **Step 3: Update docs**

Add share-link endpoints to `docs/src/api/files.md`.

- [ ] **Step 4: Final commit**
