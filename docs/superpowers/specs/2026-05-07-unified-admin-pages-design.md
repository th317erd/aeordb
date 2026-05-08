# Unified Admin Pages

**Date:** 2026-05-07
**Status:** Approved

## Problem

The four admin pages (Users, Groups, Keys, Snapshots) each implement their own layout, selection, search, and action patterns independently. Users uses tables with edit modals. Groups uses tables without edit. Keys uses cards with multi-select. Snapshots uses cards with a shared component. Groups and Keys lack edit functionality entirely. The inconsistency hurts usability and maintainability.

## Design

### Base Class: `AeorAdminPage`

A base `HTMLElement` class in `aeor-admin-page.js` (generic web component, lives in `aeordb-web-components`). All four admin pages extend it.

The base class provides:

- **Page header** — title + optional "Create" button (subclass sets `showCreateButton`)
- **Search bar** — live client-side filter; subclass provides `matchesSearch(item, query)` override
- **Card list** — renders items via subclass `renderCard(item)` override
- **Selection** — click to select/deselect, Ctrl+click toggle, Shift+click range, Ctrl+A select all, Escape clear. Mobile: tap = toggle.
- **Action bar** — hidden until selection > 0. Shows `{count} selected`, "Clear Selection", and subclass-provided action buttons via `getActionButtons(selectedItems)`.
- **Edit modal** — triggered from action bar "Edit" button. Subclass provides `renderEditForm(items)` and `submitEdit(items, formData)`. Visibility controlled by `shouldShowEditButton(selectedItems)` override.
- **Create modal** — subclass provides `renderCreateForm()` and `submitCreate(formData)`.
- **Data fetching** — subclass provides `fetchItems()`. Base handles loading state, error display, re-render.
- **Toast notifications** — success/error feedback via `window.aeorToast`

No shadow DOM. Light DOM with CSS classes from shared `components.css`.

**Subclass contract** — methods to override:

| Method | Required | Purpose |
|---|---|---|
| `fetchItems()` | Yes | Return array of items from API |
| `getItemId(item)` | Yes | Return unique identifier for selection tracking |
| `renderCard(item)` | Yes | Return HTML string for one card |
| `matchesSearch(item, query)` | Yes | Return boolean for client-side filter |
| `getActionButtons(selectedItems)` | Yes | Return HTML string for action bar buttons |
| `shouldShowEditButton(selectedItems)` | Yes | Return boolean |
| `renderCreateForm()` | If create enabled | Return HTML string for create modal body |
| `submitCreate(formData)` | If create enabled | API call, return result or throw |
| `renderEditForm(items)` | Yes | Return HTML string for edit modal body |
| `submitEdit(items, formData)` | Yes | API call(s), return result or throw |
| `onPostCreate(result)` | No | Override for custom post-create behavior (e.g. Keys shows the generated key) |
| `updateCardSelection(cardEl, isSelected)` | No | Override for custom selection visuals (e.g. Snapshots sets `selected` attribute) |

**Keyboard handling**: Ctrl+A and Escape are only captured when focus is NOT in the search input. When the search bar is focused, Ctrl+A selects text and Escape clears the search.

### Subclasses

#### AeorUsersPage (`<aeor-users>`)

- **Create**: Yes (username, email)
- **Edit**: Single select only. Editable: username, email, is_active (checkbox)
- **Bulk action**: "Deactivate" via `<aeor-confirm-button>`
- **Card**: name, email, active/inactive badge
- **Search**: matches name, email
- **`shouldShowEditButton`**: `items.length === 1`

#### AeorGroupsPage (`<aeor-groups>`)

- **Create**: Yes (name, query field/operator/value, default allow/deny via crudlify)
- **Edit**: Single or multi select. Single: all fields editable. Multi: name shows "(multiple)" disabled, only default allow/deny (crudlify) editable.
- **Bulk action**: "Delete" via `<aeor-confirm-button>`
- **Card**: name, crudlify permission flags (visual), query summary (e.g. "tags has admin")
- **Search**: matches name, query value
- **`shouldShowEditButton`**: `items.length >= 1`
- **Name resolution**: Groups with "user:UUID" names are lazily resolved to usernames for display. Post-render enrichment — `renderCard` shows the raw name, then a follow-up async call updates the card with the resolved display name.

#### AeorKeysPage (`<aeor-keys>`)

- **Create**: Yes (optional user selector, label, expires in days)
- **Edit**: Single select only. Editable: label only.
- **Bulk action**: "Revoke" via `<aeor-confirm-button>` (only for non-revoked keys)
- **Card**: label, key ID (truncated), status badge (active/revoked/expired/current session), created/expires dates
- **Search**: matches label, key ID, user ID
- **`shouldShowEditButton`**: `items.length === 1`
- **`onPostCreate`**: Overridden to show the generated API key in the modal for copy-once display before closing
- **Current session**: The portal sets a `currentKeyId` property so the card can badge the active session's key

#### AeorSnapshotsPage (`<aeor-snapshots>`)

- **Create**: No (create button hidden)
- **Edit**: Single select only. Editable: name (rename).
- **Bulk action**: "Delete" via `<aeor-confirm-button>`
- **Card**: delegates to `<aeor-snapshot-card>` component. Card's built-in Delete and Restore buttons are removed (`deletable` and `restorable` attributes omitted) — those actions live on the action bar instead.
- **Additional action**: "Restore" on single select (action bar only)
- **Search**: matches name, ID
- **`shouldShowEditButton`**: `items.length === 1`
- **`updateCardSelection`**: Overridden to set/remove the `selected` attribute on `<aeor-snapshot-card>` elements

### Action Bar Behavior

Sits between search bar and card list. Invisible until selection > 0.

**Common actions** (from base class):
- `{count} selected` label
- "Clear Selection" button

**Per-subclass actions** via `getActionButtons(selectedItems)`:

| Selection | Users | Groups | Keys | Snapshots |
|---|---|---|---|---|
| 1 item | Edit, Deactivate | Edit, Delete | Edit, Revoke | Edit, Restore, Delete |
| N items | Deactivate N | Edit, Delete N | Revoke N | Delete N |

Destructive actions use `<aeor-confirm-button>`. Non-destructive actions (Edit) use a regular button.

### Server-Side Additions

Two new endpoints for edit support:

#### PATCH /versions/snapshots/{name}

Rename a snapshot.

```json
{ "name": "new-snapshot-name" }
```

Returns `200 { "renamed": true, "from": "old-name", "to": "new-name" }`.

#### PATCH /admin/api-keys/{key_id}

Update an API key's label.

```json
{ "label": "Production API Key" }
```

Returns `200 { "updated": true, "key_id": "...", "label": "Production API Key" }`.

Existing endpoints used unchanged:
- `POST /admin/users` — create user
- `PATCH /admin/users/{id}` — update user
- `DELETE /admin/users/{id}` — deactivate user
- `POST /admin/groups` — create group
- `PATCH /admin/groups/{name}` — update group
- `DELETE /admin/groups/{name}` — delete group
- `POST /admin/api-keys` — create key
- `DELETE /admin/api-keys/{key_id}` — revoke key
- `DELETE /versions/snapshots/{name}` — delete snapshot
- `POST /versions/restore` — restore snapshot

### File Structure

**New files (aeordb-web-components — shared with sync client):**
- `aeordb-web-components/components/aeor-admin-page.js` — base class
- `aeordb-web-components/components/aeor-admin-page.css` — card list, action bar, search bar styles
- `aeordb-web-components/components/aeor-keys-page.js` — Keys subclass (used by sync client)

**Rewritten files (portal-only — import base from /shared/components/):**
- `aeordb-lib/src/portal/users.mjs` — extends AeorAdminPage
- `aeordb-lib/src/portal/groups.mjs` — extends AeorAdminPage
- `aeordb-lib/src/portal/snapshots.mjs` — extends AeorAdminPage

**Modified files:**
- `aeordb-lib/src/server/portal_routes.rs` — include_str + routes for aeor-admin-page.js and aeor-keys-page.js
- `aeordb-lib/src/server/engine_routes.rs` — add PATCH /versions/snapshots/{name} handler
- `aeordb-lib/src/server/admin_routes.rs` — add PATCH /admin/api-keys/{key_id} handler
- `aeordb-lib/src/server/mod.rs` — register two new routes
- `aeordb-web-components/styles/components.css` — append admin page CSS
- `aeordb-lib/src/portal/index.html` — remove old admin page inline styles
- `aeordb-lib/src/portal/keys.mjs` — deleted (replaced by aeordb-web-components/components/aeor-keys-page.js)

### Mobile Behavior

- Cards stack full-width on narrow screens
- Action bar wraps buttons naturally (flexbox wrap)
- Tap = toggle selection (no Ctrl/Shift on mobile)
- Search bar full-width
- Modals use existing `<aeor-modal>` which is already responsive
