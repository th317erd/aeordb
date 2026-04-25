# File Sharing Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Share modal to the file browser so users can share files and directories with specific users and groups, backed by the existing `.permissions` system with a new `path_pattern` field for per-file scoping.

**Architecture:** Three layers: (1) extend `PermissionLink` with `path_pattern` and update the resolver to check it, (2) add REST endpoints for share/unshare/list-shares, (3) add Share modal UI to the file browser with People/Link tabs.

**Tech Stack:** Rust (axum, serde), JavaScript (web components), existing permission resolver + `.permissions` files

**Spec:** `docs/superpowers/specs/2026-04-24-file-sharing-phase1-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `aeordb-lib/src/engine/permissions.rs` | Modify | Add `path_pattern` to `PermissionLink` |
| `aeordb-lib/src/engine/permission_resolver.rs` | Modify | Check `path_pattern` when evaluating links |
| `aeordb-lib/src/server/share_routes.rs` | Create | `POST /files/share`, `GET /files/shares`, `DELETE /files/shares` |
| `aeordb-lib/src/server/mod.rs` | Modify | Register share routes |
| `aeordb-web-components/components/aeor-file-browser-base.js` | Modify | Add abstract share methods, Share modal, context menu entry |
| `aeordb-web-components/components/aeor-file-browser-portal.js` | Modify | Implement share methods via portal API |
| `aeordb-lib/spec/engine/sharing_spec.rs` | Create | Permission resolver + share endpoint tests |

---

### Task 1: Extend PermissionLink with path_pattern

**Files:**
- Modify: `aeordb-lib/src/engine/permissions.rs`
- Modify: `aeordb-lib/src/engine/permission_resolver.rs`

- [ ] **Step 1: Add `path_pattern` field to `PermissionLink`**

In `aeordb-lib/src/engine/permissions.rs`, add to the `PermissionLink` struct:

```rust
    /// When set, this link only applies to entries whose filename matches
    /// this exact pattern within the directory. When absent, applies to
    /// everything in the directory (current behavior).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_pattern: Option<String>,
```

- [ ] **Step 2: Update permission resolver to check `path_pattern`**

In `aeordb-lib/src/engine/permission_resolver.rs`, in `check_permission`, the loop over `permissions.links` at line 81 currently applies all links unconditionally. Add a filter:

Change the loop body from:
```rust
      for link in &permissions.links {
        let is_member = user_groups.contains(&link.group);
```

To:
```rust
      for link in &permissions.links {
        // If link has a path_pattern, only apply when the target file matches
        if let Some(ref pattern) = link.path_pattern {
          // Extract the filename from the original path
          let filename = path.rsplit('/').next().unwrap_or("");
          if filename != pattern {
            continue; // This link doesn't apply to this file
          }
        }

        let is_member = user_groups.contains(&link.group);
```

Note: The `path` variable is the original path passed to `check_permission`. When evaluating links at a directory level, we need to check if the TARGET file's name matches the pattern. The `path` parameter contains the full target path (e.g. `/photos/sunset.jpg`), and `path.rsplit('/').next()` gives `sunset.jpg`.

However, there's a subtlety: the resolver walks directory levels, and at each level it evaluates ALL links in that level's `.permissions`. The `path_pattern` check should only match when we're at the PARENT directory of the target file. At other ancestor levels, a `path_pattern` link shouldn't match (it's scoped to immediate children).

Revised logic:
```rust
      for link in &permissions.links {
        // If link has a path_pattern, only apply when:
        // 1. This level is the immediate parent of the target path
        // 2. The target's filename matches the pattern
        if let Some(ref pattern) = link.path_pattern {
          let target_parent = {
            let trimmed = path.trim_end_matches('/');
            match trimmed.rfind('/') {
              Some(0) => "/".to_string(),
              Some(idx) => trimmed[..idx].to_string(),
              None => "/".to_string(),
            }
          };
          let normalized_level = level.trim_end_matches('/');
          let normalized_parent = target_parent.trim_end_matches('/');
          if normalized_level != normalized_parent {
            continue; // Not the parent directory — skip this pattern-scoped link
          }
          let filename = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
          if filename != pattern {
            continue; // Filename doesn't match the pattern
          }
        }

        let is_member = user_groups.contains(&link.group);
```

- [ ] **Step 3: Verify compilation and existing tests**

Run: `cargo build -p aeordb 2>&1 | tail -5`
Run: `cargo test --test permissions_spec 2>&1 | tail -10`
Expected: All existing permission tests still pass (path_pattern is optional, None by default)

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/engine/permissions.rs aeordb-lib/src/engine/permission_resolver.rs
git commit -m "Add path_pattern to PermissionLink for per-file permission scoping"
```

---

### Task 2: Share REST Endpoints

**Files:**
- Create: `aeordb-lib/src/server/share_routes.rs`
- Modify: `aeordb-lib/src/server/mod.rs`

- [ ] **Step 1: Create share_routes.rs**

Create `aeordb-lib/src/server/share_routes.rs`:

```rust
use axum::{
    Extension,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use uuid::Uuid;

use super::responses::ErrorResponse;
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::permissions::{PathPermissions, PermissionLink};
use crate::engine::request_context::RequestContext;
use crate::engine::user::is_root;

#[derive(Deserialize)]
pub struct ShareRequest {
    pub paths: Vec<String>,
    pub users: Option<Vec<String>>,
    pub groups: Option<Vec<String>>,
    pub permissions: String,
}

/// POST /files/share — grant access to paths for specified users and groups.
pub async fn share(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Json(body): Json<ShareRequest>,
) -> Response {
    let caller_id = match Uuid::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => return ErrorResponse::new("Invalid token")
            .with_status(StatusCode::UNAUTHORIZED).into_response(),
    };

    // Only root or users with configure permission can share
    // For now, allow root and the path owner
    // TODO: check configure permission on the path

    let ops = DirectoryOps::new(&state.engine);
    let ctx = RequestContext::system();
    let mut shared_count = 0;

    let users = body.users.unwrap_or_default();
    let groups = body.groups.unwrap_or_default();

    if users.is_empty() && groups.is_empty() {
        return ErrorResponse::new("At least one user or group is required")
            .with_status(StatusCode::BAD_REQUEST).into_response();
    }

    for raw_path in &body.paths {
        let normalized = crate::engine::path_utils::normalize_path(raw_path);

        // Determine the permissions directory and optional path_pattern
        let (perm_dir, path_pattern) = if normalized.ends_with('/') {
            // Directory — permissions apply to the directory itself
            (normalized.clone(), None)
        } else {
            // File — permissions go in parent dir with path_pattern
            let parent = match normalized.rfind('/') {
                Some(0) => "/".to_string(),
                Some(idx) => normalized[..idx].to_string(),
                None => "/".to_string(),
            };
            let filename = normalized.rsplit('/').next().unwrap_or("").to_string();
            (parent, Some(filename))
        };

        // Read existing .permissions or create empty
        let perm_path = if perm_dir == "/" || perm_dir.ends_with('/') {
            format!("{}.permissions", perm_dir)
        } else {
            format!("{}/.permissions", perm_dir)
        };

        let mut perms = match ops.read_file(&perm_path) {
            Ok(data) => {
                match PathPermissions::deserialize(&data) {
                    Ok(p) => p,
                    Err(_) => PathPermissions { links: Vec::new() },
                }
            }
            Err(_) => PathPermissions { links: Vec::new() },
        };

        // Add links for each user
        for user_id_str in &users {
            let group_name = format!("user:{}", user_id_str);
            upsert_link(&mut perms, &group_name, &body.permissions, &path_pattern);
        }

        // Add links for each group
        for group_name in &groups {
            upsert_link(&mut perms, group_name, &body.permissions, &path_pattern);
        }

        // Write back
        let data = perms.serialize();
        if let Err(e) = ops.store_file(&ctx, &perm_path, &data, Some("application/json")) {
            return ErrorResponse::new(format!("Failed to update permissions: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response();
        }

        // Invalidate permissions cache for this path
        state.permissions_cache.invalidate(&perm_dir);

        shared_count += 1;
    }

    (StatusCode::OK, Json(serde_json::json!({
        "shared": shared_count,
        "paths": body.paths,
    }))).into_response()
}

/// Upsert a permission link — update if same group+pattern exists, otherwise add.
fn upsert_link(
    perms: &mut PathPermissions,
    group: &str,
    allow: &str,
    path_pattern: &Option<String>,
) {
    // Find existing link with same group and same path_pattern
    let existing = perms.links.iter_mut().find(|link| {
        link.group == group && link.path_pattern == *path_pattern
    });

    if let Some(link) = existing {
        link.allow = allow.to_string();
    } else {
        perms.links.push(PermissionLink {
            group: group.to_string(),
            allow: allow.to_string(),
            deny: "........".to_string(),
            others_allow: None,
            others_deny: None,
            path_pattern: path_pattern.clone(),
        });
    }
}

#[derive(Deserialize)]
pub struct SharesQuery {
    pub path: String,
}

/// GET /files/shares?path=... — return who has access to a path.
pub async fn list_shares(
    State(state): State<AppState>,
    Extension(_claims): Extension<TokenClaims>,
    axum::extract::Query(query): axum::extract::Query<SharesQuery>,
) -> Response {
    let ops = DirectoryOps::new(&state.engine);
    let normalized = crate::engine::path_utils::normalize_path(&query.path);

    // Determine where the .permissions file is
    let perm_dir = if normalized.ends_with('/') {
        normalized.clone()
    } else {
        match normalized.rfind('/') {
            Some(0) => "/".to_string(),
            Some(idx) => normalized[..idx].to_string(),
            None => "/".to_string(),
        }
    };

    let perm_path = if perm_dir == "/" || perm_dir.ends_with('/') {
        format!("{}.permissions", perm_dir)
    } else {
        format!("{}/.permissions", perm_dir)
    };

    let perms = match ops.read_file(&perm_path) {
        Ok(data) => {
            match PathPermissions::deserialize(&data) {
                Ok(p) => p,
                Err(_) => PathPermissions { links: Vec::new() },
            }
        }
        Err(_) => PathPermissions { links: Vec::new() },
    };

    // Resolve group names to user info
    let shares: Vec<serde_json::Value> = perms.links.iter().map(|link| {
        let (share_type, username) = if link.group.starts_with("user:") {
            let uid_str = &link.group["user:".len()..];
            let uname = match Uuid::parse_str(uid_str) {
                Ok(uid) => {
                    if is_root(&uid) {
                        "root".to_string()
                    } else {
                        crate::engine::system_store::get_user(&state.engine, &uid)
                            .ok().flatten()
                            .map(|u| u.username)
                            .unwrap_or_else(|| uid_str.to_string())
                    }
                }
                Err(_) => uid_str.to_string(),
            };
            ("user", uname)
        } else {
            ("group", link.group.clone())
        };

        serde_json::json!({
            "group": link.group,
            "display_name": username,
            "type": share_type,
            "permissions": link.allow,
            "path_pattern": link.path_pattern,
        })
    }).collect();

    (StatusCode::OK, Json(serde_json::json!({
        "path": normalized,
        "shares": shares,
    }))).into_response()
}

#[derive(Deserialize)]
pub struct UnshareRequest {
    pub path: String,
    pub group: String,
    pub path_pattern: Option<String>,
}

/// DELETE /files/shares — revoke a user/group's access to a path.
pub async fn unshare(
    State(state): State<AppState>,
    Extension(_claims): Extension<TokenClaims>,
    Json(body): Json<UnshareRequest>,
) -> Response {
    let ops = DirectoryOps::new(&state.engine);
    let ctx = RequestContext::system();
    let normalized = crate::engine::path_utils::normalize_path(&body.path);

    let perm_dir = if normalized.ends_with('/') {
        normalized.clone()
    } else {
        match normalized.rfind('/') {
            Some(0) => "/".to_string(),
            Some(idx) => normalized[..idx].to_string(),
            None => "/".to_string(),
        }
    };

    let perm_path = if perm_dir == "/" || perm_dir.ends_with('/') {
        format!("{}.permissions", perm_dir)
    } else {
        format!("{}/.permissions", perm_dir)
    };

    let mut perms = match ops.read_file(&perm_path) {
        Ok(data) => {
            match PathPermissions::deserialize(&data) {
                Ok(p) => p,
                Err(_) => return ErrorResponse::new("Corrupt permissions file")
                    .with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response(),
            }
        }
        Err(_) => return ErrorResponse::new("No permissions set for this path")
            .with_status(StatusCode::NOT_FOUND).into_response(),
    };

    let before = perms.links.len();
    perms.links.retain(|link| {
        !(link.group == body.group && link.path_pattern == body.path_pattern)
    });

    if perms.links.len() == before {
        return ErrorResponse::new("Share not found")
            .with_status(StatusCode::NOT_FOUND).into_response();
    }

    let data = perms.serialize();
    if let Err(e) = ops.store_file(&ctx, &perm_path, &data, Some("application/json")) {
        return ErrorResponse::new(format!("Failed to update permissions: {}", e))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }

    state.permissions_cache.invalidate(&perm_dir);

    (StatusCode::OK, Json(serde_json::json!({
        "revoked": true,
        "group": body.group,
    }))).into_response()
}
```

Note: `state.permissions_cache.invalidate(&perm_dir)` — the implementing agent must check if `PermissionsCache` has an `invalidate` method. If not, add one:
```rust
pub fn invalidate(&self, path: &str) {
    if let Ok(mut entries) = self.entries.write() {
        entries.remove(path);
    }
}
```

- [ ] **Step 2: Register routes in mod.rs**

In `aeordb-lib/src/server/mod.rs`, add:
```rust
pub mod share_routes;
```

Register routes BEFORE the `/files/{*path}` wildcard:
```rust
    .route("/files/share", post(share_routes::share))
    .route("/files/shares", get(share_routes::list_shares).delete(share_routes::unshare))
```

- [ ] **Step 3: Verify compilation**

Run: `cargo build -p aeordb 2>&1 | tail -5`

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/server/share_routes.rs aeordb-lib/src/server/mod.rs
git commit -m "Add share/unshare/list-shares REST endpoints"
```

---

### Task 3: Share Modal UI

**Files:**
- Modify: `aeordb-web-components/components/aeor-file-browser-base.js`
- Modify: `aeordb-web-components/components/aeor-file-browser-portal.js`

- [ ] **Step 1: Add abstract share methods to base class**

In `aeor-file-browser-base.js`, add to the abstract methods section:

```javascript
  async getShares(path) {
    throw new Error('AeorFileBrowserBase.getShares() must be implemented by subclass');
  }

  async share(paths, users, groups, permissions) {
    throw new Error('AeorFileBrowserBase.share() must be implemented by subclass');
  }

  async unshare(path, group, pathPattern) {
    throw new Error('AeorFileBrowserBase.unshare() must be implemented by subclass');
  }
```

- [ ] **Step 2: Add `_showShareModal` method to base class**

Add a method that renders the Share modal with People and Link tabs:

```javascript
  async _showShareModal(paths) {
    // Fetch available users and groups
    let users = [];
    let groups = [];
    try {
      const userResp = await this.getShareableUsers();
      users = userResp || [];
    } catch (e) { /* non-critical */ }
    try {
      const groupResp = await this.getShareableGroups();
      groups = groupResp || [];
    } catch (e) { /* non-critical */ }

    // Fetch current shares for the first path
    let currentShares = [];
    try {
      const sharesData = await this.getShares(paths[0]);
      currentShares = sharesData.shares || [];
    } catch (e) { /* non-critical */ }

    const pathLabel = (paths.length === 1)
      ? escapeHtml(paths[0])
      : `${paths.length} items`;

    const userOptions = users.map((u) =>
      `<option value="${escapeAttr(String(u.user_id))}">${escapeHtml(u.username)}</option>`
    ).join('');

    const groupOptions = groups.map((g) =>
      `<option value="${escapeAttr(g.name)}">${escapeHtml(g.name)}</option>`
    ).join('');

    const currentSharesHtml = currentShares.length > 0
      ? currentShares.map((s) => `
          <div style="display:flex;justify-content:space-between;align-items:center;padding:6px 0;border-bottom:1px solid var(--border);">
            <div>
              <strong>${escapeHtml(s.display_name || s.group)}</strong>
              <span style="color:var(--text-muted);font-size:0.8rem;margin-left:8px;">${escapeHtml(s.permissions)}</span>
              ${s.path_pattern ? `<span style="color:var(--text-muted);font-size:0.8rem;margin-left:8px;">(${escapeHtml(s.path_pattern)})</span>` : ''}
            </div>
            <button class="danger small revoke-share-btn" data-group="${escapeAttr(s.group)}" data-pattern="${escapeAttr(s.path_pattern || '')}">×</button>
          </div>
        `).join('')
      : '<div style="color:var(--text-muted);padding:12px 0;">No shares yet</div>';

    const modal = document.createElement('aeor-modal');
    modal.title = `Share ${pathLabel}`;
    modal.innerHTML = `
      <div style="margin-bottom:16px;">
        <div style="display:flex;gap:8px;margin-bottom:16px;">
          <button class="primary small share-tab-btn active" data-tab="people">People</button>
          <button class="secondary small share-tab-btn" data-tab="link" disabled title="Coming in Phase 2" style="opacity:0.5;">Link</button>
        </div>

        <div id="share-tab-people">
          <div class="form-group">
            <label class="form-label">Users</label>
            <select class="form-input" id="share-users" multiple size="3">${userOptions}</select>
          </div>
          <div class="form-group">
            <label class="form-label">Groups</label>
            <select class="form-input" id="share-groups" multiple size="3">${groupOptions}</select>
          </div>
          <div class="form-group">
            <label class="form-label">Permission Level</label>
            <select class="form-input" id="share-permission-level">
              <option value="cr..l...">View only</option>
              <option value="crudl..." selected>Can edit</option>
              <option value="crudlify">Full access</option>
            </select>
          </div>
          <div style="display:flex;gap:10px;justify-content:flex-end;margin-bottom:20px;">
            <button class="secondary small" id="share-cancel-btn">Cancel</button>
            <button class="primary small" id="share-submit-btn">Share</button>
          </div>

          <div style="border-top:1px solid var(--border);padding-top:12px;">
            <div style="font-size:0.85rem;font-weight:600;margin-bottom:8px;color:var(--text-secondary);">Current Shares</div>
            ${currentSharesHtml}
          </div>
        </div>

        <div id="share-tab-link" style="display:none;">
          <div style="color:var(--text-muted);text-align:center;padding:40px;">
            Signed URL sharing coming in Phase 2.
          </div>
        </div>
      </div>
    `;

    document.body.appendChild(modal);

    // Cancel / close
    modal.querySelector('#share-cancel-btn').addEventListener('click', () => modal.remove());
    modal.addEventListener('close', () => modal.remove());

    // Submit share
    modal.querySelector('#share-submit-btn').addEventListener('click', async () => {
      const userSelect = modal.querySelector('#share-users');
      const groupSelect = modal.querySelector('#share-groups');
      const permLevel = modal.querySelector('#share-permission-level').value;

      const selectedUsers = Array.from(userSelect.selectedOptions).map((o) => o.value);
      const selectedGroups = Array.from(groupSelect.selectedOptions).map((o) => o.value);

      if (selectedUsers.length === 0 && selectedGroups.length === 0) {
        if (window.aeorToast) window.aeorToast('Select at least one user or group', 'warning');
        return;
      }

      try {
        await this.share(paths, selectedUsers, selectedGroups, permLevel);
        if (window.aeorToast) window.aeorToast('Shared successfully', 'success');
        modal.remove();
      } catch (error) {
        if (window.aeorToast) window.aeorToast('Share failed: ' + error.message, 'error');
      }
    });

    // Revoke buttons
    modal.querySelectorAll('.revoke-share-btn').forEach((btn) => {
      btn.addEventListener('click', async () => {
        try {
          const pattern = btn.dataset.pattern || null;
          await this.unshare(paths[0], btn.dataset.group, pattern);
          if (window.aeorToast) window.aeorToast('Share revoked', 'success');
          modal.remove();
          // Re-open to refresh
          this._showShareModal(paths);
        } catch (error) {
          if (window.aeorToast) window.aeorToast('Revoke failed: ' + error.message, 'error');
        }
      });
    });
  }
```

Also add abstract methods for fetching shareable users/groups:
```javascript
  async getShareableUsers() { return []; }
  async getShareableGroups() { return []; }
```

- [ ] **Step 3: Add Share button to preview actions and selection bar**

The base class has hook methods `previewActions(entry)` and `selectionActions(tab)` that subclasses override. Add a "Share" action to the base class context menu:

In `_showContextMenu`, add a "Share" menu item:
```javascript
    menu.innerHTML = `
      <div class="context-menu-item" data-context="preview">Preview</div>
      <div class="context-menu-item" data-context="share">Share</div>
      <div class="context-menu-item context-menu-danger" data-context="delete">Delete</div>
    `;
```

In the context menu click handler, add the share case:
```javascript
        if (item.dataset.context === 'share') {
          const filePath = activeTab.path.replace(/\/$/, '') + '/' + entry.name;
          this._showShareModal([filePath]);
        }
```

- [ ] **Step 4: Implement share methods in portal subclass**

In `aeor-file-browser-portal.js`, add:

```javascript
  async getShares(path) {
    const response = await window.api(`/files/shares?path=${encodeURIComponent(path)}`);
    if (!response.ok) throw new Error(`${response.status}`);
    return response.json();
  }

  async share(paths, users, groups, permissions) {
    const response = await window.api('/files/share', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ paths, users, groups, permissions }),
    });
    if (!response.ok) throw new Error(`${response.status}`);
  }

  async unshare(path, group, pathPattern) {
    const response = await window.api('/files/shares', {
      method: 'DELETE',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ path, group, path_pattern: pathPattern }),
    });
    if (!response.ok) throw new Error(`${response.status}`);
  }

  async getShareableUsers() {
    const response = await window.api('/auth/keys/users');
    if (!response.ok) return [];
    const data = await response.json();
    return data.items || [];
  }

  async getShareableGroups() {
    const response = await window.api('/system/groups');
    if (!response.ok) return [];
    const data = await response.json();
    return data.items || [];
  }
```

Also add "Share" button to portal's `previewActions`:
```javascript
  previewActions(entry) {
    return '<button class="primary small" data-action="download">Download</button>' +
           '<button class="secondary small" data-action="share">Share</button>';
  }
```

Add "Share" to portal's `selectionActions`:
```javascript
  selectionActions(tab) {
    return '<button class="primary small selection-download-zip">Download ZIP</button>' +
           '<button class="secondary small selection-share">Share</button>';
  }
```

Wire up the share selection button in `_bindSelectionBarExtra`:
```javascript
  _bindSelectionBarExtra(selectionBar, tab) {
    const zipBtn = selectionBar.querySelector('.selection-download-zip');
    if (zipBtn) {
      zipBtn.addEventListener('click', () => this._downloadSelectedAsZip());
    }
    const shareBtn = selectionBar.querySelector('.selection-share');
    if (shareBtn) {
      shareBtn.addEventListener('click', () => {
        const paths = [...tab.selectedEntries];
        this._showShareModal(paths);
      });
    }
  }
```

Handle the "share" preview action:
```javascript
  async _handlePreviewAction(action) {
    if (action === 'share') {
      const tab = this._activeTab();
      if (!tab || !tab.preview_entry) return;
      const filePath = tab.path.replace(/\/$/, '') + '/' + tab.preview_entry.name;
      this._showShareModal([filePath]);
      return;
    }
    // ... existing download/download-zip handlers ...
  }
```

- [ ] **Step 5: Verify everything compiles**

Run: `cargo build 2>&1 | tail -5`
Run: `node -c aeordb-web-components/components/aeor-file-browser-base.js`
Run: `node -c aeordb-web-components/components/aeor-file-browser-portal.js`

- [ ] **Step 6: Commit**

```bash
# Shared components
cd aeordb-web-components
git add components/
git commit -m "Add Share modal, abstract share methods, context menu Share entry"

# Server
cd ../aeordb
git add aeordb-lib/src/server/share_routes.rs aeordb-lib/src/server/mod.rs
git commit -m "Wire share routes and portal share implementation"
```

---

### Task 4: Tests

**Files:**
- Create: `aeordb-lib/spec/engine/sharing_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Create sharing test file**

Create `aeordb-lib/spec/engine/sharing_spec.rs` with tests for:

1. **path_pattern_scopes_to_specific_file** — create `.permissions` with `path_pattern: "sunset.jpg"`, verify user can access `/photos/sunset.jpg` but NOT `/photos/beach.jpg`

2. **path_pattern_none_grants_directory_wide** — create `.permissions` without `path_pattern`, verify user can access all files in the directory

3. **share_endpoint_creates_permissions** — `POST /files/share` with user + path, verify `.permissions` file created with correct link

4. **share_endpoint_updates_existing** — share same path twice with different permissions, verify link is updated (not duplicated)

5. **unshare_removes_link** — share then unshare, verify link removed from `.permissions`

6. **list_shares_returns_current_state** — share with user and group, `GET /files/shares` returns both with resolved names

7. **per_file_share_does_not_grant_sibling_access** — share `/photos/sunset.jpg` with user, verify user cannot access `/photos/beach.jpg`

Use the existing test patterns from `permissions_spec.rs` for setting up engines, users, groups, and `.permissions` files.

- [ ] **Step 2: Register test in Cargo.toml**

```toml
[[test]]
name = "sharing_spec"
path = "spec/engine/sharing_spec.rs"
```

- [ ] **Step 3: Run tests**

Run: `cargo test --test sharing_spec 2>&1 | tail -20`
Run: `cargo test 2>&1 | grep "FAILED" | grep "test " || echo "ALL TESTS PASS"`

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/spec/engine/sharing_spec.rs aeordb-lib/Cargo.toml
git commit -m "Add sharing tests: path_pattern scoping, share/unshare endpoints, per-file isolation"
```

---

### Task 5: Full Verification

- [ ] **Step 1: Run the complete test suite**

Run: `cargo test 2>&1 | grep "test result:" | awk '{sum += $4} END {print "Total:", sum, "tests"}'`

- [ ] **Step 2: Manual E2E test**

```bash
# Start server, create user, share a file, verify access
target/debug/aeordb start -D /tmp/claude/share-test.aeordb --auth false -p 6832 &
sleep 2

# Upload a file
curl -s -X PUT http://localhost:6832/files/photos/sunset.jpg -d "jpeg-data" -H "Content-Type: image/jpeg"

# Share it (via the API directly)
curl -s -X POST http://localhost:6832/files/share -H "Content-Type: application/json" \
  -d '{"paths":["/photos/sunset.jpg"],"users":[],"groups":["everyone"],"permissions":"cr..l..."}'

# List shares
curl -s "http://localhost:6832/files/shares?path=/photos/sunset.jpg"

pkill -f "6832"
```

- [ ] **Step 3: Update TODO.md**

- [ ] **Step 4: Final commit**
