# Snapshots Admin Page Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Admin-only page for managing database snapshots — list, search, delete (batch), and restore HEAD to a selected snapshot. Clones the Keys page card layout.

**Architecture:** New `snapshots.mjs` portal page following the Keys page pattern (card layout, selection, search). Uses existing snapshot API endpoints. Long-press buttons for destructive actions (delete, restore).

**Tech Stack:** JavaScript (portal .mjs page), Rust (portal_routes.rs for serving)

---

### Task 1: Create snapshots.mjs page

**Files:**
- Create: `aeordb-lib/src/portal/snapshots.mjs`

- [ ] **Step 1: Create the snapshots page**

Create `aeordb-lib/src/portal/snapshots.mjs`. This is a clone of `keys.mjs` adapted for snapshots:

```javascript
'use strict';

import { escapeHtml } from '/shared/utils.js';
import '/shared/components/aeor-long-press-button.js';

class AeorSnapshots extends HTMLElement {
  constructor() {
    super();
    this._snapshots = [];
    this._error = null;
    this._searchQuery = '';
    this._selectedIds = new Set();
    this._lastSelectedAnchor = null;
  }

  connectedCallback() {
    this.render();
    this._fetchSnapshots();
  }

  render() {
    this.innerHTML = `
      <div class="page-header">
        <h1 class="page-title">Snapshots</h1>
      </div>
      <div style="margin-bottom:16px;">
        <input class="form-input" id="snapshots-search" type="text"
          placeholder="Search snapshots by name or ID...">
      </div>
      <div id="snapshots-error"></div>
      <div id="snapshots-content"></div>
    `;

    this.querySelector('#snapshots-search').addEventListener('input', (event) => {
      this._searchQuery = event.target.value;
      this.renderContent();
    });
  }

  async _fetchSnapshots() {
    try {
      const response = await window.api('/versions/snapshots');
      if (!response.ok) throw new Error(`Failed to fetch snapshots (${response.status})`);
      const data = await response.json();
      this._snapshots = (data.items || []).sort((a, b) => b.created_at - a.created_at);
      this._error = null;
      this.renderContent();
    } catch (error) {
      this._error = error.message;
      this.renderContent();
    }
  }

  _getDisplaySnapshots() {
    const query = this._searchQuery.trim().toLowerCase();
    if (query.length === 0) return this._snapshots;
    return this._snapshots.filter((s) => {
      return (s.name || '').toLowerCase().includes(query)
        || (s.id || '').toLowerCase().includes(query);
    });
  }

  _truncateId(id) {
    if (!id) return '\u2014';
    const str = String(id);
    if (str.length <= 16) return str;
    return str.slice(0, 8) + '\u2026' + str.slice(-8);
  }

  _timeAgo(timestamp) {
    const seconds = Math.floor((Date.now() - timestamp) / 1000);
    if (seconds < 60) return 'just now';
    const minutes = Math.floor(seconds / 60);
    if (minutes < 60) return `${minutes}m ago`;
    const hours = Math.floor(minutes / 60);
    if (hours < 24) return `${hours}h ago`;
    const days = Math.floor(hours / 24);
    if (days < 30) return `${days}d ago`;
    const months = Math.floor(days / 30);
    return `${months}mo ago`;
  }

  renderContent() {
    const contentContainer = this.querySelector('#snapshots-content');
    const errorContainer = this.querySelector('#snapshots-error');
    if (!contentContainer || !errorContainer) return;

    if (this._error) {
      errorContainer.innerHTML = `<div class="alert alert-error">${escapeHtml(this._error)}</div>`;
    } else {
      errorContainer.innerHTML = '';
    }

    const displaySnapshots = this._getDisplaySnapshots();

    if (displaySnapshots.length === 0 && !this._error) {
      contentContainer.innerHTML = `
        <div class="card" style="text-align:center;padding:40px;">
          <div style="color:#8b949e;">No snapshots found.</div>
        </div>
      `;
      return;
    }

    const newestId = this._snapshots.length > 0 ? this._snapshots[0].id : null;

    contentContainer.innerHTML = `
      <style>
        .snap-list { display: flex; flex-direction: column; gap: 1px; }

        .snap-row {
          display: grid;
          grid-template-columns: 1fr auto;
          align-items: center;
          gap: 12px;
          padding: 12px 16px;
          background: var(--card);
          border: 1px solid var(--border);
          border-radius: 6px;
          margin-bottom: 4px;
          cursor: pointer;
          outline: none;
          user-select: none;
        }

        .snap-row:hover { border-color: var(--accent); }
        .snap-row.selected { background: rgba(249, 115, 22, 0.15); border-color: var(--accent); }

        .snap-selection-bar {
          display: flex;
          align-items: center;
          gap: 12px;
          padding: 8px 16px;
          height: 44px;
          background: var(--card-hover, #21262d);
          border: 1px solid var(--border);
          border-radius: 6px;
          margin-bottom: 8px;
          font-size: 0.9rem;
          color: var(--text-muted);
          box-sizing: border-box;
          visibility: hidden;
        }

        .snap-selection-bar .sel-count { font-weight: 600; color: var(--text); }

        .snap-info { min-width: 0; }

        .snap-name {
          font-weight: 600;
          color: var(--text);
          margin-bottom: 2px;
          display: flex;
          align-items: center;
          gap: 8px;
          flex-wrap: wrap;
        }

        .snap-id {
          font-family: var(--font-mono);
          font-size: 0.78rem;
          color: var(--text-muted);
          display: flex;
          align-items: center;
          gap: 6px;
        }

        .snap-id .copy-btn {
          cursor: pointer;
          opacity: 0.5;
          font-size: 0.7rem;
        }
        .snap-id .copy-btn:hover { opacity: 1; }

        .snap-meta {
          font-size: 0.78rem;
          color: var(--text-muted);
          margin-top: 4px;
        }

        .snap-actions {
          display: flex;
          gap: 8px;
          align-items: center;
          flex-shrink: 0;
        }

        @media (max-width: 768px) {
          .snap-actions { flex-direction: column; gap: 4px; }
        }
      </style>

      <div class="snap-selection-bar" id="snap-selection-bar">&nbsp;</div>
      <div class="snap-list">
        ${displaySnapshots.map((snap) => {
          const isSelected = this._selectedIds.has(snap.id);
          const isCurrent = snap.id === newestId;
          const created = snap.created_at ? new Date(snap.created_at).toLocaleString() : '\u2014';
          const age = snap.created_at ? this._timeAgo(snap.created_at) : '';

          return `
            <div class="snap-row ${isSelected ? 'selected' : ''}" data-snap-id="${escapeHtml(snap.id || '')}">
              <div class="snap-info">
                <div class="snap-name">
                  ${escapeHtml(snap.name || 'Unnamed')}
                  ${isCurrent ? '<span class="badge badge-active">current</span>' : `<span class="badge" style="background:rgba(139,148,158,0.15);color:var(--text-muted);">${escapeHtml(age)}</span>`}
                </div>
                <div class="snap-id">
                  <span title="${escapeHtml(snap.id || '')}">${escapeHtml(this._truncateId(snap.id))}</span>
                  <span class="copy-btn" data-copy-id="${escapeHtml(snap.id || '')}" title="Copy ID">&#128203;</span>
                </div>
                <div class="snap-meta">Created ${escapeHtml(created)}</div>
              </div>
              <div class="snap-actions">
                <aeor-long-press-button class="snap-restore-btn" label="Restore" confirmed-text="Restored!" duration="1000" style="--lpb-bg:var(--accent,#f97316);--lpb-text:#fff;--lpb-fill:var(--success,#3fb950);--lpb-border:var(--accent,#f97316);"></aeor-long-press-button>
                <aeor-long-press-button class="snap-delete-btn" label="Delete" confirmed-text="Deleted!" duration="1000" style="--lpb-fill:var(--danger,#f85149);--lpb-text:var(--danger,#f85149);"></aeor-long-press-button>
              </div>
            </div>
          `;
        }).join('')}
      </div>
    `;

    this._bindRowEvents(contentContainer, displaySnapshots);
    this._updateSelectionBar();
  }

  _bindRowEvents(container, displaySnapshots) {
    // Row click — selection
    container.querySelectorAll('.snap-row').forEach((row) => {
      row.addEventListener('click', (event) => {
        if (event.target.closest('aeor-long-press-button')) return;
        if (event.target.closest('.copy-btn')) return;

        const snapId = row.dataset.snapId;
        const index = displaySnapshots.findIndex((s) => s.id === snapId);
        const isMobile = window.innerWidth <= 768;
        const isCtrl = isMobile || event.ctrlKey || event.metaKey;
        const isShift = !isMobile && event.shiftKey;

        if (!isCtrl && !isShift) {
          this._selectedIds.clear();
          this._selectedIds.add(snapId);
          this._lastSelectedAnchor = snapId;
        } else if (isCtrl) {
          if (this._selectedIds.has(snapId))
            this._selectedIds.delete(snapId);
          else
            this._selectedIds.add(snapId);
          this._lastSelectedAnchor = snapId;
        } else if (isShift) {
          const anchorIndex = this._lastSelectedAnchor
            ? displaySnapshots.findIndex((s) => s.id === this._lastSelectedAnchor)
            : 0;
          const anchor = (anchorIndex >= 0) ? anchorIndex : 0;
          const start = Math.min(anchor, index);
          const end = Math.max(anchor, index);
          for (let i = start; i <= end; i++) {
            if (displaySnapshots[i]) this._selectedIds.add(displaySnapshots[i].id);
          }
        }

        this._updateSelectionVisual(container);
        this._updateSelectionBar();
      });
    });

    // Per-card Restore button
    container.querySelectorAll('.snap-restore-btn').forEach((btn) => {
      btn.addEventListener('confirm', () => {
        const row = btn.closest('.snap-row');
        if (row) this._restoreSnapshot(row.dataset.snapId);
      });
    });

    // Per-card Delete button
    container.querySelectorAll('.snap-delete-btn').forEach((btn) => {
      btn.addEventListener('confirm', () => {
        const row = btn.closest('.snap-row');
        if (row) this._deleteSnapshot(row.dataset.snapId);
      });
    });

    // Copy ID buttons
    container.querySelectorAll('.copy-btn').forEach((btn) => {
      btn.addEventListener('click', (event) => {
        event.stopPropagation();
        const id = btn.dataset.copyId;
        if (id) {
          navigator.clipboard.writeText(id).then(() => {
            btn.textContent = '\u2713';
            setTimeout(() => { btn.textContent = '\uD83D\uDCCB'; }, 1500);
          }).catch(() => {});
        }
      });
    });

    // Ctrl+A and Escape
    this.setAttribute('tabindex', '0');
    this.style.outline = 'none';
    const keydownHandler = (event) => {
      if ((event.ctrlKey || event.metaKey) && event.key === 'a') {
        event.preventDefault();
        for (const s of displaySnapshots) this._selectedIds.add(s.id);
        if (displaySnapshots.length > 0)
          this._lastSelectedAnchor = displaySnapshots[displaySnapshots.length - 1].id;
        this._updateSelectionVisual(container);
        this._updateSelectionBar();
      } else if (event.key === 'Escape') {
        this._selectedIds.clear();
        this._lastSelectedAnchor = null;
        this._updateSelectionVisual(container);
        this._updateSelectionBar();
      }
    };
    if (this._keydownHandler) this.removeEventListener('keydown', this._keydownHandler);
    this._keydownHandler = keydownHandler;
    this.addEventListener('keydown', keydownHandler);
  }

  _updateSelectionVisual(container) {
    container.querySelectorAll('.snap-row').forEach((row) => {
      if (this._selectedIds.has(row.dataset.snapId))
        row.classList.add('selected');
      else
        row.classList.remove('selected');
    });
  }

  _updateSelectionBar() {
    const bar = this.querySelector('#snap-selection-bar');
    if (!bar) return;

    if (this._selectedIds.size > 0) {
      bar.innerHTML = `
        <span class="sel-count">${this._selectedIds.size} selected</span>
        <aeor-long-press-button id="delete-selected-btn" label="Delete ${this._selectedIds.size} Snapshot${this._selectedIds.size > 1 ? 's' : ''}" confirmed-text="Deleted!" duration="1000" style="--lpb-fill:var(--danger,#f85149);--lpb-text:var(--danger,#f85149);"></aeor-long-press-button>
        <button class="button button-small" id="clear-selection-btn">Clear Selection</button>
      `;
      bar.style.visibility = 'visible';

      const deleteBtn = bar.querySelector('#delete-selected-btn');
      if (deleteBtn) {
        deleteBtn.addEventListener('confirm', () => this._deleteSelected());
      }
      bar.querySelector('#clear-selection-btn').addEventListener('click', () => {
        this._selectedIds.clear();
        this._lastSelectedAnchor = null;
        const container = this.querySelector('#snapshots-content');
        if (container) this._updateSelectionVisual(container);
        this._updateSelectionBar();
      });
    } else {
      bar.innerHTML = '&nbsp;';
      bar.style.visibility = 'hidden';
    }
  }

  async _restoreSnapshot(id) {
    try {
      const response = await window.api('/versions/restore', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ id }),
      });
      if (!response.ok) {
        const err = await response.json().catch(() => ({ error: 'Restore failed' }));
        throw new Error(err.error || `HTTP ${response.status}`);
      }
      if (window.aeorToast) window.aeorToast('HEAD restored to snapshot', 'success');
      await this._fetchSnapshots();
    } catch (error) {
      if (window.aeorToast) window.aeorToast('Restore failed: ' + error.message, 'error');
    }
  }

  async _deleteSnapshot(id) {
    try {
      const response = await window.api(`/versions/snapshots/${encodeURIComponent(id)}`, {
        method: 'DELETE',
      });
      if (!response.ok) {
        const err = await response.json().catch(() => ({ error: 'Delete failed' }));
        throw new Error(err.error || `HTTP ${response.status}`);
      }
      this._snapshots = this._snapshots.filter((s) => s.id !== id);
      this._selectedIds.delete(id);
      this.renderContent();
    } catch (error) {
      if (window.aeorToast) window.aeorToast('Delete failed: ' + error.message, 'error');
    }
  }

  async _deleteSelected() {
    const ids = [...this._selectedIds];
    for (const id of ids) {
      try {
        await window.api(`/versions/snapshots/${encodeURIComponent(id)}`, { method: 'DELETE' });
        this._snapshots = this._snapshots.filter((s) => s.id !== id);
        this._selectedIds.delete(id);
      } catch (error) {
        if (window.aeorToast) window.aeorToast('Delete failed: ' + error.message, 'error');
      }
    }
    this._lastSelectedAnchor = null;
    this.renderContent();
  }
}

customElements.define('aeor-snapshots', AeorSnapshots);
```

- [ ] **Step 2: Verify syntax**

```bash
node -c aeordb-lib/src/portal/snapshots.mjs
```

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/src/portal/snapshots.mjs
git commit -m "feat: snapshots admin page (card layout, search, restore, batch delete)"
```

---

### Task 2: Wire up the page in the portal

**Files:**
- Modify: `aeordb-lib/src/portal/index.html`
- Modify: `aeordb-lib/src/portal/app.mjs`
- Modify: `aeordb-lib/src/server/portal_routes.rs`

- [ ] **Step 1: Add nav link in index.html**

Find the sidebar nav links (around line 588-593). Add "Snapshots" between "Keys" and "Settings":

```html
<a class="nav-link" data-page="snapshots" href="?page=snapshots">Snapshots</a>
```

- [ ] **Step 2: Import snapshots.mjs in app.mjs**

Add after the keys import (around line 12):

```javascript
import '/snapshots.mjs';
```

Add to the pageMap (around line 253):

```javascript
'snapshots': 'aeor-snapshots',
```

- [ ] **Step 3: Serve snapshots.mjs in portal_routes.rs**

Add the include_str constant (after the PORTAL_KEYS_MJS line, around line 28):

```rust
const PORTAL_SNAPSHOTS_MJS: &str = include_str!("../portal/snapshots.mjs");
```

Add the route match (after the keys.mjs match, around line 60):

```rust
"snapshots.mjs" => (PORTAL_SNAPSHOTS_MJS, "application/javascript; charset=utf-8"),
```

- [ ] **Step 4: Build**

```bash
cargo build --release 2>&1 | tail -5
```

- [ ] **Step 5: Commit**

```bash
git commit -am "feat: wire snapshots page into portal nav, routing, and serving"
```
