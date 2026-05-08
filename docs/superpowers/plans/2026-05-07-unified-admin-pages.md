# Unified Admin Pages Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace four inconsistent admin pages (Users, Groups, Keys, Snapshots) with a shared `AeorAdminPage` base class providing unified card layout, selection, search, action bar, and create/edit modals.

**Architecture:** `AeorAdminPage` base HTMLElement in `aeordb-web-components` with OOP inheritance. Four thin subclasses override `fetchItems()`, `renderCard()`, `getActionButtons()`, etc. Two new server endpoints for snapshot rename and key label update. Base class handles all selection, search, action bar, and modal logic.

**Tech Stack:** Vanilla JS web components (no framework), Rust/Axum server endpoints, CSS

**Spec:** `docs/superpowers/specs/2026-05-07-unified-admin-pages-design.md`

**Reference:** Read `IMPLEMENTATION-GUIDE.md` in `/home/wyatt/Projects/aeor-web-components/` before writing any component code. It defines the element builder pattern, accessibility standards (rem units, prefers-reduced-motion, focus-visible), and CSS conventions used across all components.

---

### Task 1: Server — PATCH /versions/snapshots/{name} (rename snapshot)

**Files:**
- Modify: `aeordb-lib/src/engine/version_manager.rs`
- Modify: `aeordb-lib/src/server/engine_routes.rs`
- Modify: `aeordb-lib/src/server/mod.rs`

- [ ] **Step 1: Add `rename_snapshot` to VersionManager**

In `aeordb-lib/src/engine/version_manager.rs`, add this method to `impl<'a> VersionManager<'a>`:

```rust
  /// Rename a snapshot. Creates a new snapshot entry with the new name
  /// and the same root hash/metadata, then deletes the old one.
  pub fn rename_snapshot(&self, ctx: &RequestContext, old_name: &str, new_name: &str) -> EngineResult<SnapshotInfo> {
    // Load existing snapshot
    let old_key = self.snapshot_key(old_name)?;
    let entry = self.engine.get_entry(&old_key)?;
    let Some((header, _key, value)) = entry else {
      return Err(EngineError::NotFound(format!("Snapshot not found: {}", old_name)));
    };

    let hash_length = self.engine.hash_algo().hash_length();
    let old_snapshot = SnapshotInfo::deserialize(&value, hash_length, header.entry_version)?;

    // Check new name doesn't already exist
    let new_key = self.snapshot_key(new_name)?;
    if self.engine.has_entry(&new_key)? && !self.engine.is_entry_deleted(&new_key)? {
      return Err(EngineError::AlreadyExists(format!("Snapshot already exists: {}", new_name)));
    }

    // Create new snapshot with same root hash and metadata
    let new_snapshot = SnapshotInfo {
      name: new_name.to_string(),
      root_hash: old_snapshot.root_hash,
      created_at: old_snapshot.created_at,
      metadata: old_snapshot.metadata,
    };

    let new_value = new_snapshot.serialize(hash_length)?;
    self.engine.store_entry_typed(
      crate::engine::entry_type::EntryType::Snapshot,
      &new_key,
      &new_value,
      crate::engine::kv_store::KV_TYPE_SNAPSHOT,
    )?;

    // Delete old snapshot entry
    self.engine.mark_entry_deleted(&old_key)?;

    Ok(new_snapshot)
  }
```

- [ ] **Step 2: Add the route handler in engine_routes.rs**

In `aeordb-lib/src/server/engine_routes.rs`, add this handler after `snapshot_delete`:

```rust
/// PATCH /versions/snapshots/{name} -- rename a snapshot (requires root).
pub async fn snapshot_rename(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(id_or_name): Path<String>,
  Json(payload): Json<serde_json::Value>,
) -> Response {
  let user_id = match uuid::Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => {
      return (StatusCode::FORBIDDEN, Json(serde_json::json!({
        "error": "Invalid user ID"
      }))).into_response();
    }
  };
  if !is_root(&user_id) {
    return (StatusCode::FORBIDDEN, Json(serde_json::json!({
      "error": "Only root user can rename snapshots"
    }))).into_response();
  }

  let new_name = match payload.get("name").and_then(|v| v.as_str()) {
    Some(name) if !name.is_empty() => name,
    _ => {
      return ErrorResponse::new("Missing or empty 'name' field")
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let version_manager = VersionManager::new(&state.engine);

  // Resolve the snapshot (accepts ID or name)
  let snapshot = match version_manager.resolve_snapshot(&id_or_name) {
    Ok(s) => s,
    Err(_) => {
      return ErrorResponse::new(format!("Snapshot not found: '{}'", id_or_name))
        .with_status(StatusCode::NOT_FOUND)
        .into_response();
    }
  };

  match version_manager.rename_snapshot(&ctx, &snapshot.name, new_name) {
    Ok(_) => {
      (StatusCode::OK, Json(serde_json::json!({
        "renamed": true,
        "from": snapshot.name,
        "to": new_name,
      }))).into_response()
    }
    Err(EngineError::AlreadyExists(msg)) => {
      ErrorResponse::new(msg)
        .with_status(StatusCode::CONFLICT)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Failed to rename snapshot '{}': {}", snapshot.name, error);
      ErrorResponse::new(format!("Failed to rename snapshot: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}
```

- [ ] **Step 3: Register the route in mod.rs**

In `aeordb-lib/src/server/mod.rs`, find the snapshot route registration (`.route("/versions/snapshots/{name}", delete(engine_routes::snapshot_delete))`) and add `.patch`:

```rust
.route("/versions/snapshots/{name}", delete(engine_routes::snapshot_delete)
                                     .patch(engine_routes::snapshot_rename))
```

- [ ] **Step 4: Build and verify**

Run: `cargo build -p aeordb --lib 2>&1 | grep "^error" | head -10`
Expected: No errors.

- [ ] **Step 5: Commit**

```bash
git add aeordb-lib/src/engine/version_manager.rs aeordb-lib/src/server/engine_routes.rs aeordb-lib/src/server/mod.rs
git commit -m "feat: PATCH /versions/snapshots/{name} endpoint for snapshot rename"
```

---

### Task 2: Server — PATCH /admin/api-keys/{key_id} (update key label)

**Files:**
- Modify: `aeordb-lib/src/server/admin_routes.rs`
- Modify: `aeordb-lib/src/server/mod.rs`

- [ ] **Step 1: Add the handler in admin_routes.rs**

In `aeordb-lib/src/server/admin_routes.rs`, add this handler. Follow the pattern of `update_group` (around line 352). Place it after the existing API key handlers:

```rust
/// PATCH /admin/api-keys/{key_id} -- update an API key's label.
pub async fn update_api_key(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(key_id_string): Path<String>,
  Json(payload): Json<serde_json::Value>,
) -> Response {
  let user_id = match uuid::Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => {
      return (StatusCode::FORBIDDEN, Json(serde_json::json!({
        "error": "Invalid user ID"
      }))).into_response();
    }
  };
  if !crate::engine::user::is_root(&user_id) {
    return (StatusCode::FORBIDDEN, Json(serde_json::json!({
      "error": "Only root user can update API keys"
    }))).into_response();
  }

  let key_uuid = match uuid::Uuid::parse_str(&key_id_string) {
    Ok(id) => id,
    Err(_) => {
      return super::responses::ErrorResponse::new("Invalid key ID format")
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  // Read the existing key record
  let ops = crate::engine::DirectoryOps::new(&state.engine);
  let path = format!("/.aeordb-system/api-keys/{}", key_uuid);
  let data = match ops.read_file(&path) {
    Ok(data) => data,
    Err(crate::engine::EngineError::NotFound(_)) => {
      return super::responses::ErrorResponse::new(format!("API key not found: {}", key_id_string))
        .with_status(StatusCode::NOT_FOUND)
        .into_response();
    }
    Err(error) => {
      return super::responses::ErrorResponse::new(format!("Failed to read API key: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  let mut record: crate::auth::api_key::ApiKeyRecord = match serde_json::from_slice(&data) {
    Ok(r) => r,
    Err(e) => {
      return super::responses::ErrorResponse::new(format!("Corrupt API key record: {}", e))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  // Apply label update
  if let Some(label) = payload.get("label").and_then(|v| v.as_str()) {
    record.label = Some(label.to_string());
  }

  // Save back
  let ctx = crate::engine::RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  match crate::engine::system_store::store_api_key(&state.engine, &ctx, &record) {
    Ok(()) => {
      state.api_key_cache.evict(&key_id_string);
      (StatusCode::OK, Json(serde_json::json!({
        "updated": true,
        "key_id": key_id_string,
        "label": record.label,
      }))).into_response()
    }
    Err(error) => {
      super::responses::ErrorResponse::new(format!("Failed to update API key: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}
```

- [ ] **Step 2: Register the route in mod.rs**

In `aeordb-lib/src/server/mod.rs`, find the API key routes. Look for the admin key route pattern and add a PATCH route. If there's an existing `/admin/api-keys/{key_id}` route with DELETE, add `.patch` to it. If not, add a new route:

```rust
.route("/admin/api-keys/{key_id}", patch(admin_routes::update_api_key))
```

Check the existing route registration to see if there's already a route for this path with DELETE (for revoke). If so, chain them:

```rust
.route("/admin/api-keys/{key_id}", delete(admin_routes::revoke_api_key_admin)
                                   .patch(admin_routes::update_api_key))
```

- [ ] **Step 3: Build and verify**

Run: `cargo build -p aeordb --lib 2>&1 | grep "^error" | head -10`
Expected: No errors.

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/server/admin_routes.rs aeordb-lib/src/server/mod.rs
git commit -m "feat: PATCH /admin/api-keys/{key_id} endpoint for key label update"
```

---

### Task 3: Create AeorAdminPage base class

**Files:**
- Create: `aeordb-web-components/components/aeor-admin-page.js`

This is the core of the entire feature. Read `/home/wyatt/Projects/aeor-web-components/IMPLEMENTATION-GUIDE.md` before writing.

- [ ] **Step 1: Create the base class**

Create `aeordb-web-components/components/aeor-admin-page.js`. The file should be ~400-500 lines. Here is the complete implementation:

```javascript
'use strict';

import '/shared/components/aeor-modal.js';
import '/shared/components/aeor-confirm-button.js';

/**
 * AeorAdminPage — Base class for admin list pages (Users, Groups, Keys, Snapshots).
 *
 * Provides: page header, search bar, card list, multi-select, action bar,
 * create/edit modals, loading/error states.
 *
 * Subclasses MUST override:
 *   - get title()
 *   - get showCreateButton()
 *   - fetchItems()
 *   - getItemId(item)
 *   - renderCard(item)
 *   - matchesSearch(item, query)
 *   - getActionButtons(selectedItems)
 *   - shouldShowEditButton(selectedItems)
 *   - renderCreateForm()      (if showCreateButton)
 *   - submitCreate(formData)  (if showCreateButton)
 *   - renderEditForm(items)
 *   - submitEdit(items, formData)
 *
 * Subclasses MAY override:
 *   - onPostCreate(result)         — custom post-create behavior
 *   - updateCardSelection(el, sel) — custom selection visuals
 *   - onItemsLoaded(items)         — post-fetch hook (e.g. async name resolution)
 */
export class AeorAdminPage extends HTMLElement {
  constructor() {
    super();
    this._items = [];
    this._selectedIds = new Set();
    this._lastSelectedAnchor = null;
    this._searchQuery = '';
    this._error = null;
    this._loading = false;
  }

  // ── Subclass contract (MUST override) ──────────────────────────────

  get title() { return 'Admin'; }
  get showCreateButton() { return true; }

  async fetchItems() { return []; }
  getItemId(item) { return item.id || item.name; }
  renderCard(item) { return `<div>${JSON.stringify(item)}</div>`; }
  matchesSearch(item, query) { return true; }
  getActionButtons(selectedItems) { return ''; }
  shouldShowEditButton(selectedItems) { return selectedItems.length === 1; }
  renderCreateForm() { return ''; }
  async submitCreate(formData) { }
  renderEditForm(items) { return ''; }
  async submitEdit(items, formData) { }

  // ── Subclass hooks (MAY override) ──────────────────────────────────

  onPostCreate(result) { /* default: close modal and refresh */ }
  onItemsLoaded(items) { /* default: no-op */ }

  updateCardSelection(cardEl, isSelected) {
    if (isSelected) {
      cardEl.classList.add('selected');
    } else {
      cardEl.classList.remove('selected');
    }
  }

  // ── Lifecycle ──────────────────────────────────────────────────────

  connectedCallback() {
    this._render();
    this._loadItems();
  }

  // ── Render ─────────────────────────────────────────────────────────

  _render() {
    this.innerHTML = `
      <div class="page-header">
        <h1 class="page-title">${this._esc(this.title)}</h1>
        ${this.showCreateButton ? '<button class="admin-create-btn primary">Create</button>' : ''}
      </div>
      <div class="admin-search-wrap">
        <input class="form-input admin-search" type="text" placeholder="Search...">
      </div>
      <div class="admin-action-bar invisible"></div>
      <div class="admin-error"></div>
      <div class="admin-list"></div>
    `;

    // Search
    this.querySelector('.admin-search').addEventListener('input', (e) => {
      this._searchQuery = e.target.value.trim().toLowerCase();
      this._renderList();
    });

    // Create button
    const createBtn = this.querySelector('.admin-create-btn');
    if (createBtn) {
      createBtn.addEventListener('click', () => this._openCreateModal());
    }

    // Keyboard shortcuts
    this._keydownHandler = (e) => {
      // Don't capture when search input is focused
      if (document.activeElement === this.querySelector('.admin-search')) return;

      if ((e.ctrlKey || e.metaKey) && e.key === 'a') {
        e.preventDefault();
        const visible = this._getVisibleItems();
        for (const item of visible) this._selectedIds.add(this.getItemId(item));
        if (visible.length > 0) this._lastSelectedAnchor = this.getItemId(visible[visible.length - 1]);
        this._updateSelectionVisuals();
        this._updateActionBar();
      } else if (e.key === 'Escape') {
        this._clearSelection();
      }
    };
    this.setAttribute('tabindex', '0');
    this.style.outline = 'none';
    this.addEventListener('keydown', this._keydownHandler);
  }

  disconnectedCallback() {
    if (this._keydownHandler) {
      this.removeEventListener('keydown', this._keydownHandler);
    }
  }

  // ── Data loading ───────────────────────────────────────────────────

  async _loadItems() {
    this._loading = true;
    this._renderList();

    try {
      this._items = await this.fetchItems();
      this._error = null;
      await this.onItemsLoaded(this._items);
    } catch (error) {
      this._error = error.message;
      this._items = [];
    }

    this._loading = false;
    this._renderList();
  }

  // ── List rendering ─────────────────────────────────────────────────

  _getVisibleItems() {
    if (!this._searchQuery) return this._items;
    return this._items.filter((item) => this.matchesSearch(item, this._searchQuery));
  }

  _renderList() {
    const listEl = this.querySelector('.admin-list');
    const errorEl = this.querySelector('.admin-error');
    if (!listEl || !errorEl) return;

    // Error
    if (this._error) {
      errorEl.innerHTML = `<div class="alert alert-error">${this._esc(this._error)}</div>`;
    } else {
      errorEl.innerHTML = '';
    }

    // Loading
    if (this._loading && this._items.length === 0) {
      listEl.innerHTML = '<div class="admin-empty">&nbsp;</div>';
      return;
    }

    const visible = this._getVisibleItems();

    if (visible.length === 0) {
      listEl.innerHTML = `<div class="admin-empty">${this._searchQuery ? 'No matches found.' : 'No items.'}</div>`;
      return;
    }

    listEl.innerHTML = visible.map((item) => {
      const id = this.getItemId(item);
      const isSelected = this._selectedIds.has(id);
      return `<div class="admin-card${isSelected ? ' selected' : ''}" data-item-id="${this._escAttr(String(id))}">${this.renderCard(item)}</div>`;
    }).join('');

    this._bindCardEvents(listEl, visible);
  }

  // ── Card events (selection) ────────────────────────────────────────

  _bindCardEvents(listEl, visibleItems) {
    listEl.querySelectorAll('.admin-card').forEach((cardEl) => {
      cardEl.addEventListener('click', (e) => {
        // Ignore clicks on buttons/inputs inside the card
        if (e.target.closest('button') || e.target.closest('aeor-confirm-button') ||
            e.target.closest('input') || e.target.closest('a')) return;

        const itemId = cardEl.dataset.itemId;
        const index = visibleItems.findIndex((item) => String(this.getItemId(item)) === itemId);
        const isMobile = window.innerWidth <= 768;
        const isCtrl = isMobile || e.ctrlKey || e.metaKey;
        const isShift = !isMobile && e.shiftKey;

        if (!isCtrl && !isShift) {
          this._selectedIds.clear();
          this._selectedIds.add(itemId);
          this._lastSelectedAnchor = itemId;
        } else if (isCtrl) {
          if (this._selectedIds.has(itemId)) {
            this._selectedIds.delete(itemId);
          } else {
            this._selectedIds.add(itemId);
          }
          this._lastSelectedAnchor = itemId;
        } else if (isShift) {
          const anchorIndex = this._lastSelectedAnchor
            ? visibleItems.findIndex((item) => String(this.getItemId(item)) === this._lastSelectedAnchor)
            : 0;
          const anchor = (anchorIndex >= 0) ? anchorIndex : 0;
          const start = Math.min(anchor, index);
          const end = Math.max(anchor, index);
          for (let i = start; i <= end; i++) {
            if (visibleItems[i]) this._selectedIds.add(String(this.getItemId(visibleItems[i])));
          }
        }

        this._updateSelectionVisuals();
        this._updateActionBar();
      });
    });
  }

  // ── Selection visuals ──────────────────────────────────────────────

  _updateSelectionVisuals() {
    this.querySelectorAll('.admin-card').forEach((cardEl) => {
      const isSelected = this._selectedIds.has(cardEl.dataset.itemId);
      this.updateCardSelection(cardEl, isSelected);
    });
  }

  _clearSelection() {
    this._selectedIds.clear();
    this._lastSelectedAnchor = null;
    this._updateSelectionVisuals();
    this._updateActionBar();
  }

  // ── Action bar ─────────────────────────────────────────────────────

  _getSelectedItems() {
    return this._items.filter((item) => this._selectedIds.has(String(this.getItemId(item))));
  }

  _updateActionBar() {
    const bar = this.querySelector('.admin-action-bar');
    if (!bar) return;

    if (this._selectedIds.size === 0) {
      bar.innerHTML = '';
      bar.classList.add('invisible');
      return;
    }

    const selectedItems = this._getSelectedItems();

    bar.innerHTML = `
      <span class="admin-sel-count">${this._selectedIds.size} selected</span>
      ${this.shouldShowEditButton(selectedItems) ? '<button class="secondary small admin-edit-btn">Edit</button>' : ''}
      ${this.getActionButtons(selectedItems)}
      <button class="secondary small admin-clear-btn">Clear Selection</button>
    `;
    bar.classList.remove('invisible');

    // Bind action bar events
    const editBtn = bar.querySelector('.admin-edit-btn');
    if (editBtn) editBtn.addEventListener('click', () => this._openEditModal(selectedItems));

    const clearBtn = bar.querySelector('.admin-clear-btn');
    if (clearBtn) clearBtn.addEventListener('click', () => this._clearSelection());

    // Let subclass bind its custom action buttons
    this._bindActionBarEvents(bar, selectedItems);
  }

  /** Subclasses override to bind event listeners on their custom action buttons. */
  _bindActionBarEvents(bar, selectedItems) { }

  // ── Create modal ───────────────────────────────────────────────────

  _openCreateModal() {
    const formHtml = this.renderCreateForm();
    const modal = document.createElement('aeor-modal');
    modal.setAttribute('title', `Create ${this.title.replace(/s$/, '')}`);
    modal.innerHTML = `
      <div class="admin-modal-form">
        ${formHtml}
        <div class="modal-footer-actions">
          <button class="secondary small admin-modal-cancel">Cancel</button>
          <button class="primary small admin-modal-submit">Create</button>
        </div>
      </div>
    `;

    document.body.appendChild(modal);
    modal.open();

    modal.querySelector('.admin-modal-cancel').addEventListener('click', () => {
      modal.close();
      modal.remove();
    });

    modal.querySelector('.admin-modal-submit').addEventListener('click', async () => {
      try {
        const result = await this.submitCreate(modal);
        this.onPostCreate(result);
        if (!this._postCreateHandled) {
          modal.close();
          modal.remove();
          if (window.aeorToast) window.aeorToast('Created successfully', 'success');
          await this._loadItems();
        }
        this._postCreateHandled = false;
      } catch (error) {
        if (window.aeorToast) window.aeorToast('Create failed: ' + error.message, 'error');
      }
    });

    modal.addEventListener('close', () => modal.remove());
  }

  // ── Edit modal ─────────────────────────────────────────────────────

  _openEditModal(items) {
    const formHtml = this.renderEditForm(items);
    const noun = this.title.replace(/s$/, '');
    const modalTitle = items.length === 1
      ? `Edit ${noun}`
      : `Edit ${items.length} ${this.title}`;

    const modal = document.createElement('aeor-modal');
    modal.setAttribute('title', modalTitle);
    modal.innerHTML = `
      <div class="admin-modal-form">
        ${formHtml}
        <div class="modal-footer-actions">
          <button class="secondary small admin-modal-cancel">Cancel</button>
          <button class="primary small admin-modal-submit">Save</button>
        </div>
      </div>
    `;

    document.body.appendChild(modal);
    modal.open();

    modal.querySelector('.admin-modal-cancel').addEventListener('click', () => {
      modal.close();
      modal.remove();
    });

    modal.querySelector('.admin-modal-submit').addEventListener('click', async () => {
      try {
        await this.submitEdit(items, modal);
        modal.close();
        modal.remove();
        if (window.aeorToast) window.aeorToast('Updated successfully', 'success');
        this._clearSelection();
        await this._loadItems();
      } catch (error) {
        if (window.aeorToast) window.aeorToast('Update failed: ' + error.message, 'error');
      }
    });

    modal.addEventListener('close', () => modal.remove());
  }

  // ── Utilities ──────────────────────────────────────────────────────

  _esc(str) {
    if (!str) return '';
    const d = document.createElement('div');
    d.textContent = str;
    return d.innerHTML;
  }

  _escAttr(str) {
    return (str || '').replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  }
}
```

- [ ] **Step 2: Verify file created**

Run: `ls -la aeordb-web-components/components/aeor-admin-page.js`
Expected: File exists.

- [ ] **Step 3: Commit**

```bash
git add aeordb-web-components/components/aeor-admin-page.js
git commit -m "feat: AeorAdminPage base class with selection, search, action bar, modals"
```

---

### Task 4: Create AeorAdminPage CSS

**Files:**
- Create: `aeordb-web-components/components/aeor-admin-page.css`
- Modify: `aeordb-web-components/styles/components.css`

- [ ] **Step 1: Create the CSS file**

Create `aeordb-web-components/components/aeor-admin-page.css`:

```css
/* aeor-admin-page — Shared admin list page styles */

.admin-search-wrap {
  margin-bottom: 0.5rem;
}

.admin-search {
  width: 100%;
}

/* Action bar */
.admin-action-bar {
  display: flex;
  align-items: center;
  gap: 0.75rem;
  flex-wrap: wrap;
  padding: 0.5rem 1rem;
  background: var(--card-hover, #21262d);
  border: 1px solid var(--border, #30363d);
  border-radius: 0.375rem;
  margin-bottom: 0.5rem;
  font-size: 0.9rem;
  color: var(--text-muted, #8b949e);
  box-sizing: border-box;
}

.admin-action-bar.invisible {
  display: none;
}

.admin-sel-count {
  font-weight: 600;
  color: var(--text, #e6edf3);
}

/* Card list */
.admin-list {
  display: flex;
  flex-direction: column;
  gap: 0.125rem;
}

/* Individual card */
.admin-card {
  padding: 0.625rem 0.75rem;
  background: var(--card, #161b22);
  border: 1px solid var(--border, #30363d);
  border-radius: 0.375rem;
  cursor: pointer;
  user-select: none;
  transition: border-color 0.15s, background 0.15s;
}

.admin-card:hover {
  border-color: var(--accent, #f97316);
}

.admin-card.selected {
  background: rgba(249, 115, 22, 0.15);
  border-color: var(--accent, #f97316);
}

/* Card content layout */
.admin-card-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 0.5rem;
}

.admin-card-title {
  font-weight: 600;
  font-size: 0.875rem;
  color: var(--text, #e6edf3);
  display: flex;
  align-items: center;
  gap: 0.375rem;
  flex-wrap: wrap;
}

.admin-card-meta {
  font-size: 0.75rem;
  color: var(--text-muted, #8b949e);
  margin-top: 0.25rem;
}

.admin-card-detail {
  font-size: 0.75rem;
  color: var(--text-muted, #8b949e);
  font-family: var(--font-mono, monospace);
  margin-top: 0.125rem;
}

/* Empty state */
.admin-empty {
  padding: 2rem;
  text-align: center;
  color: var(--text-muted, #8b949e);
}

/* Modal form layout */
.admin-modal-form {
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
}

@media (max-width: 768px) {
  .admin-card {
    padding: 0.5rem;
  }

  .admin-action-bar {
    padding: 0.375rem 0.5rem;
    gap: 0.5rem;
  }
}

@media (prefers-reduced-motion: reduce) {
  .admin-card {
    transition: none;
  }
}
```

- [ ] **Step 2: Append CSS to shared components.css**

Read `/home/wyatt/Projects/aeordb-workspace/aeordb-web-components/styles/components.css` to find the end of the file. Append the contents of `aeor-admin-page.css` with a header comment `/* --- aeor-admin-page.css --- */`.

- [ ] **Step 3: Commit**

```bash
git add aeordb-web-components/components/aeor-admin-page.css aeordb-web-components/styles/components.css
git commit -m "feat: admin page card layout, action bar, and modal CSS"
```

---

### Task 5: Rewrite Users page

**Files:**
- Modify: `aeordb-lib/src/portal/users.mjs`

Read the existing `users.mjs` fully before rewriting. The new version extends `AeorAdminPage` from `/shared/components/aeor-admin-page.js`.

- [ ] **Step 1: Rewrite users.mjs**

Read the current file, then replace it entirely. Key details to preserve:
- API endpoints: `GET /system/users`, `POST /system/users`, `PATCH /system/users/{id}`, `DELETE /system/users/{id}`
- User fields: `user_id` (UUID), `username`, `email`, `is_active`, `created_at`
- Create form: username, email
- Edit form: username, email, is_active checkbox
- Deactivate = DELETE (soft-delete, not permanent)

The new file imports `AeorAdminPage` and defines `AeorUsersPage extends AeorAdminPage`. Register as `<aeor-users>`. ~120-180 lines.

Use `escapeHtml` from `/shared/utils.js` for card rendering.

- [ ] **Step 2: Build and verify**

Run: `cargo build --release 2>&1 | tail -3`
Expected: Compiles (portal assets are embedded at build time).

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/src/portal/users.mjs
git commit -m "refactor: rewrite Users page on AeorAdminPage base class"
```

---

### Task 6: Rewrite Groups page

**Files:**
- Modify: `aeordb-lib/src/portal/groups.mjs`

Read the existing `groups.mjs` fully before rewriting. Key details to preserve:
- API endpoints: `GET /system/groups`, `POST /system/groups`, `PATCH /system/groups/{name}`, `DELETE /system/groups/{name}`
- Group fields: `name`, `default_allow`, `default_deny`, `query_field`, `query_operator`, `query_value`, `created_at`
- "user:UUID" group name resolution to usernames (async, best-effort via `GET /auth/keys/users`)
- Create form: name, default allow/deny (aeor-crudlify), query field/operator/value
- Edit form: same fields. Multi-edit: name disabled showing "(multiple)", only crudlify allow/deny editable
- `shouldShowEditButton`: `items.length >= 1` (allows multi-edit)
- Uses `<aeor-crudlify>` component for permission flags

Import `aeor-crudlify.js` in addition to the base class. The `onItemsLoaded` hook resolves "user:UUID" group names.

- [ ] **Step 1: Rewrite groups.mjs**

Replace the file entirely with the new implementation. ~200-250 lines.

- [ ] **Step 2: Build and verify**

Run: `cargo build --release 2>&1 | tail -3`

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/src/portal/groups.mjs
git commit -m "refactor: rewrite Groups page on AeorAdminPage base class with edit support"
```

---

### Task 7: Create Keys page (in aeordb-web-components)

**Files:**
- Create: `aeordb-web-components/components/aeor-keys-page.js`
- Delete: `aeordb-lib/src/portal/keys.mjs`

Read the existing `keys.mjs` fully before rewriting. Key details to preserve:
- API endpoints: `GET /auth/keys` (own), `GET /auth/keys/admin` (all, root only), `POST /auth/keys`, `DELETE /auth/keys/{keyId}` or `DELETE /auth/keys/admin/{keyId}`, new `PATCH /admin/api-keys/{key_id}`
- Key fields: `key_id`, `label`, `user_id`, `username`, `is_revoked`, `expires_at`, `created_at`, `rules`
- Status badges: current session, active, revoked, expired
- `currentKeyId` property set by portal's `app.mjs` to identify the active session's key
- Create form: optional user selector (loaded from `/auth/keys/users`), label, expires in days
- `onPostCreate`: shows the generated API key in the modal for copy-once before closing
- Edit form: label only
- Revoke = DELETE
- Search: lazy-loads all keys on first search (empty search = own keys only)

The new file lives at `aeordb-web-components/components/aeor-keys-page.js`. It imports the base class with a relative path: `import { AeorAdminPage } from './aeor-admin-page.js';`

Delete the old `aeordb-lib/src/portal/keys.mjs` after verifying the new one compiles.

- [ ] **Step 1: Create aeor-keys-page.js**

Write the full implementation. ~250-350 lines (Keys is the most complex subclass due to the create flow, status badges, and lazy search).

- [ ] **Step 2: Delete old keys.mjs**

```bash
rm aeordb-lib/src/portal/keys.mjs
```

- [ ] **Step 3: Commit**

```bash
git add aeordb-web-components/components/aeor-keys-page.js
git add aeordb-lib/src/portal/keys.mjs
git commit -m "refactor: rewrite Keys page on AeorAdminPage, move to aeordb-web-components"
```

---

### Task 8: Rewrite Snapshots page

**Files:**
- Modify: `aeordb-lib/src/portal/snapshots.mjs`

Read the existing `snapshots.mjs` fully before rewriting. Key details to preserve:
- API endpoints: `GET /versions/snapshots`, `POST /versions/restore`, `DELETE /versions/snapshots/{name}`, new `PATCH /versions/snapshots/{name}`
- Snapshot fields: `name`, `id` (hex root hash), `created_at`, `metadata`
- Deduplication: sort newest first, dedup by root hash (keep newest per hash)
- Current indicator: newest snapshot by timestamp
- Card rendering: delegates to `<aeor-snapshot-card>` component, but WITHOUT `deletable`/`restorable` attributes (those actions on the action bar)
- `updateCardSelection`: sets/removes `selected` attribute on `<aeor-snapshot-card>` elements
- Restore action on single select
- Edit = rename (PATCH endpoint)
- `_timeAgo` helper for relative dates

Import `aeor-snapshot-card.js` in addition to the base class.

- [ ] **Step 1: Rewrite snapshots.mjs**

Replace the file entirely. ~150-200 lines.

- [ ] **Step 2: Build and verify**

Run: `cargo build --release 2>&1 | tail -3`

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/src/portal/snapshots.mjs
git commit -m "refactor: rewrite Snapshots page on AeorAdminPage with rename support"
```

---

### Task 9: Wire up portal routes and clean up

**Files:**
- Modify: `aeordb-lib/src/server/portal_routes.rs`
- Modify: `aeordb-lib/src/portal/index.html`
- Modify: `aeordb-lib/src/portal/app.mjs`

- [ ] **Step 1: Add include_str and route for aeor-admin-page.js and aeor-keys-page.js**

In `aeordb-lib/src/server/portal_routes.rs`:

Add the include_str constants (after the existing shared component constants):
```rust
const PORTAL_SHARED_ADMIN_PAGE_JS: &str = include_str!("../portal/shared/components/aeor-admin-page.js");
const PORTAL_SHARED_KEYS_PAGE_JS: &str = include_str!("../portal/shared/components/aeor-keys-page.js");
```

Add the route matches in `portal_shared_asset` (inside the match block):
```rust
"components/aeor-admin-page.js" => (PORTAL_SHARED_ADMIN_PAGE_JS, "application/javascript; charset=utf-8"),
"components/aeor-keys-page.js" => (PORTAL_SHARED_KEYS_PAGE_JS, "application/javascript; charset=utf-8"),
```

Remove the old `PORTAL_KEYS_MJS` constant and its route in `portal_asset` (since `keys.mjs` was deleted).

- [ ] **Step 2: Update portal app.mjs imports**

In `aeordb-lib/src/portal/app.mjs`, find the keys import and update it:

```javascript
// Old:
// import '/keys.mjs';
// New:
import '/shared/components/aeor-keys-page.js';
```

Also make sure `aeor-admin-page.js` is imported (it's imported by the subclasses, but verify the base class import chain works).

- [ ] **Step 3: Clean up index.html**

In `aeordb-lib/src/portal/index.html`, remove any inline `<style>` blocks that were specific to the old admin pages (e.g., `.crudlify-row`, old table styles for users/groups). Keep styles that are used elsewhere.

Search for these and remove if they're only used by the old admin pages:
- `.crudlify-row` / `.crudlify-flag` styles (still used by `<aeor-crudlify>` component — keep these)
- Any table-specific styles only used by users/groups pages

- [ ] **Step 4: Build the release binary**

Run: `cargo build --release 2>&1 | tail -5`
Expected: Compiles cleanly.

- [ ] **Step 5: Commit**

```bash
git add aeordb-lib/src/server/portal_routes.rs aeordb-lib/src/portal/app.mjs aeordb-lib/src/portal/index.html
git commit -m "chore: wire admin page components into portal routes, clean up old styles"
```

---

### Task 10: Smoke test

**Files:** None (verification only)

- [ ] **Step 1: Start the server**

```bash
./target/release/aeordb start -D "/path/to/test.aeordb" --port 6830
```

- [ ] **Step 2: Verify each page loads and renders**

Open `http://localhost:6830` in a browser. Navigate to each admin page:
- Users — cards render, search works, create/edit modals open
- Groups — cards render with crudlify flags, search works, create/edit modals, multi-edit shows crudlify only
- Keys — cards render with status badges, search works (lazy-loads all on first search), create shows key for copy, edit label works
- Snapshots — snapshot cards render, search works, restore/delete from action bar, rename via edit

- [ ] **Step 3: Verify selection on each page**

Test on each page:
- Click to select single item
- Ctrl+click to toggle
- Shift+click for range select
- Ctrl+A to select all
- Escape to clear
- Action bar appears/disappears correctly
- Edit button shows/hides per the rules (Groups: always, others: single only)

- [ ] **Step 4: Verify new server endpoints**

```bash
# Rename a snapshot
curl -X PATCH -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"name":"renamed-snapshot"}' \
  http://localhost:6830/versions/snapshots/test-snap-1

# Update key label
curl -X PATCH -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"label":"My Updated Key"}' \
  http://localhost:6830/admin/api-keys/$KEY_ID
```

- [ ] **Step 5: Final commit**

```bash
git add -A
git commit -m "chore: unified admin pages — verified working"
```
