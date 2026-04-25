# File Sharing — Phase 1 Design Spec

**Date:** 2026-04-24
**Status:** Approved
**Phase:** 1 of 3 (People sharing + permissions UI)

## Overview

Add a Share modal to the file browser that lets users share files and directories with specific users and groups. Sharing works by updating `.permissions` files — no new access control system, just a friendly UI on top of the existing permission resolver.

Phase 2 (signed URLs / public links) and Phase 3 (email notifications + settings page) are deferred.

---

## Share Entry Points

1. **Preview panel** — "Share" button in action bar (single file/folder)
2. **Selection bar** — "Share" button when items are selected (bulk share)
3. **Context menu** — "Share" option as secondary shortcut (right-click)

---

## Share Modal

Two tabs: **"People"** and **"Link"**.

**Link tab** is greyed out in Phase 1 with "Coming soon" text.

### People Tab

**User selector** — multi-select searchable dropdown. Fetches from `GET /auth/keys/users` (already permission-scoped).

**Group selector** — multi-select searchable dropdown. Fetches from `GET /system/groups`.

**Permission level** — select box:
- "View only" → `cr..l...` (create, read, list)
- "Can edit" → `crudl...` (create, read, update, delete, list)
- "Full access" → `crudlify` (all 8 flags)
- "Custom" → reveals 8 crudlify toggle flags

**Share button** — submits the selection.

**Current shares section** — below the selectors, shows who already has access to this path (read from existing `.permissions`). Each entry shows the group/user name, their permission level, and a remove (×) button to revoke.

---

## Backend

### `POST /files/share`

Grant access to paths for specified users and groups.

**Request:**
```json
{
  "paths": ["/photos/shoot-2026/sunset.jpg", "/photos/shoot-2026/beach.jpg"],
  "users": ["6f94eecf-b136-47b4-9b47-c20f781f1f5b"],
  "groups": ["design-team"],
  "permissions": "crudl..."
}
```

**What it does:**
1. For each path, determine the permissions directory:
   - If path is a directory → use that directory
   - If path is a file → use the parent directory
2. Read the existing `.permissions` file at that directory (or create one)
3. For each user → add/update a `PermissionLink` for group `user:{user_id}` with the specified allow flags and `deny: "........"`
4. For each group → add/update a `PermissionLink` for that group name
5. For per-file paths → set `path_pattern` on the link to the filename, so the permission only applies to that specific file within the directory
6. Write the updated `.permissions` file
7. Return `200 OK` with the updated share state

**Deduplication:** If a link for the same group (and same `path_pattern`) already exists, update its `allow` flags rather than creating a duplicate.

**Response:**
```json
{
  "shared": 2,
  "paths": ["/photos/shoot-2026/sunset.jpg", "/photos/shoot-2026/beach.jpg"]
}
```

### `GET /files/shares?path=...`

Return who has access to a specific path.

**Response:**
```json
{
  "path": "/photos/shoot-2026/",
  "shares": [
    {
      "group": "user:6f94eecf-...",
      "username": "wyatt",
      "type": "user",
      "permissions": "crudl...",
      "path_pattern": null
    },
    {
      "group": "design-team",
      "username": null,
      "type": "group",
      "permissions": "cr..l...",
      "path_pattern": "sunset.jpg"
    }
  ]
}
```

Reads the `.permissions` file at the path (or parent for files), resolves group names to user names where possible (for `user:{id}` groups), and returns the list.

### `DELETE /files/shares`

Revoke a specific user/group's access to a path.

**Request:**
```json
{
  "path": "/photos/shoot-2026/",
  "group": "user:6f94eecf-...",
  "path_pattern": null
}
```

Removes the matching `PermissionLink` from the `.permissions` file.

---

## PermissionLink Extension

Add optional `path_pattern` field to enable per-file permissions:

```rust
pub struct PermissionLink {
    pub group: String,
    pub allow: String,
    pub deny: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub others_allow: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub others_deny: Option<String>,
    /// When set, this link only applies to entries matching this pattern
    /// within the directory. Supports exact filename match.
    /// When absent, applies to everything in the directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_pattern: Option<String>,
}
```

### Permission Resolver Change

In `permission_resolver.rs`, when evaluating links at a directory level, check `path_pattern`:
- If `path_pattern` is `None` → apply to all paths in this directory (current behavior)
- If `path_pattern` is `Some(pattern)` → only apply when the target file's name matches the pattern

The resolver already walks from root to target. At each level, it reads `.permissions` and evaluates links. The change is: before applying a link's allow/deny flags, check if the link has a `path_pattern` and if so, whether the current target's filename matches.

---

## File Browser UI Changes

### Base class (`aeor-file-browser-base.js`)

Add abstract methods:
```javascript
async getShares(path) { throw new Error('not implemented'); }
async share(paths, users, groups, permissions) { throw new Error('not implemented'); }
async unshare(path, group, pathPattern) { throw new Error('not implemented'); }
```

Add "Share" to `previewActions()` hook — subclasses that support sharing override to include a Share button.

Add "Share" to selection bar via `selectionActions()` hook.

Add `_showShareModal(paths)` method that renders the modal with People/Link tabs.

### Portal subclass (`aeor-file-browser-portal.js`)

Implement:
- `getShares(path)` → `GET /files/shares?path=...` via `window.api()`
- `share(paths, users, groups, permissions)` → `POST /files/share` via `window.api()`
- `unshare(path, group, pathPattern)` → `DELETE /files/shares` via `window.api()`
- Override `previewActions()` to include Share button
- Override `selectionActions()` to include Share button

### Context menu

Add "Share" option to the existing `_showContextMenu` in the base class. Clicking it calls `_showShareModal([path])`.

---

## Testing Strategy

**Share endpoint:**
- Share file with user → user can read it
- Share file with user → other files in same dir still denied
- Share directory with group → group members get access to all children
- Share with "View only" → user can read but not write/delete
- Share same path twice → updates permissions, no duplicates
- Revoke share → user loses access
- Get shares → returns correct list with resolved usernames

**Permission resolver with path_pattern:**
- Link with `path_pattern: "sunset.jpg"` → only grants access to that file
- Link without `path_pattern` → grants access to entire directory
- Multiple links with different patterns at same level → each scoped correctly
- Pattern link + directory-wide link → both apply (merge)

**UI (manual/Puppeteer):**
- Share button appears in preview panel
- Share button appears in selection bar with multiple items selected
- Modal shows People tab with user/group selectors
- Current shares section shows existing permissions
- Revoke (×) removes access
- Link tab shows "Coming soon"

---

## Out of Scope (Phase 2+)

- Signed URLs / public links (Phase 2)
- Email notifications on share (Phase 3)
- Settings page for email config (Phase 3)
- SMTP / OAuth email providers (Phase 3)
- Glob patterns in `path_pattern` (exact match only for now)
