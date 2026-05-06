'use strict';

import { escapeHtml } from '/shared/utils.js';
import '/shared/components/aeor-confirm-button.js';
import '/shared/components/aeor-snapshot-card.js';

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
      <div class="search-bar-wrap">
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
      // Sort newest first, then deduplicate by root hash (id) — keep newest per hash
      const sorted = (data.items || []).sort((a, b) => b.created_at - a.created_at);
      const seen = new Set();
      this._snapshots = sorted.filter((s) => {
        if (seen.has(s.id)) return false;
        seen.add(s.id);
        return true;
      });
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
        <div class="card empty-state">
          <div class="empty-state-message">No snapshots found.</div>
        </div>
      `;
      return;
    }

    // "current" = newest by timestamp (first in sorted array), identified by name (unique)
    const newestName = this._snapshots.length > 0 ? this._snapshots[0].name : null;

    contentContainer.innerHTML = `
      <style>
        .snap-list { display: flex; flex-direction: column; gap: 1px; }

        .snap-selection-bar {
          display: flex;
          align-items: center;
          gap: 0.75rem;
          padding: 0.5rem 1rem;
          height: 2.75rem;
          background: var(--card-hover, #21262d);
          border: 1px solid var(--border);
          border-radius: 0.375rem;
          margin-bottom: 0.5rem;
          font-size: 0.9rem;
          color: var(--text-muted);
          box-sizing: border-box;
          visibility: hidden;
        }

        .snap-selection-bar .sel-count { font-weight: 600; color: var(--text); }
      </style>

      <div class="snap-selection-bar" id="snap-selection-bar">&nbsp;</div>
      <div class="snap-list">
        ${displaySnapshots.map((snap) => {
          const isCurrent = snap.name === newestName;
          const created = snap.created_at ? 'Created ' + new Date(snap.created_at).toLocaleString() : '\u2014';
          const age = snap.created_at ? this._timeAgo(snap.created_at) : '';

          return `<aeor-snapshot-card
              name="${escapeHtml(snap.name || '')}"
              snapshot-id="${escapeHtml(snap.id || '')}"
              date="${escapeHtml(created)}"
              ${isCurrent ? 'current' : ''}
              ${!isCurrent ? `badge="${escapeHtml(age)}"` : ''}
              ${this._selectedIds.has(snap.name) ? 'selected' : ''}
              deletable
              restorable
              truncate-id
            ></aeor-snapshot-card>`;
        }).join('')}
      </div>
    `;

    this._bindRowEvents(contentContainer, displaySnapshots);
    this._updateSelectionBar();
  }

  _bindRowEvents(container, displaySnapshots) {
    // Card click — selection
    container.querySelectorAll('aeor-snapshot-card').forEach((card) => {
      card.addEventListener('click', (event) => {
        if (event.target.closest('aeor-confirm-button') || event.target.closest('.snapshot-card-copy-btn')) return;

        const snapName = card.getAttribute('name');
        const index = displaySnapshots.findIndex((s) => s.name === snapName);
        const isMobile = window.innerWidth <= 768;
        const isCtrl = isMobile || event.ctrlKey || event.metaKey;
        const isShift = !isMobile && event.shiftKey;

        if (!isCtrl && !isShift) {
          this._selectedIds.clear();
          this._selectedIds.add(snapName);
          this._lastSelectedAnchor = snapName;
        } else if (isCtrl) {
          if (this._selectedIds.has(snapName))
            this._selectedIds.delete(snapName);
          else
            this._selectedIds.add(snapName);
          this._lastSelectedAnchor = snapName;
        } else if (isShift) {
          const anchorIndex = this._lastSelectedAnchor
            ? displaySnapshots.findIndex((s) => s.name === this._lastSelectedAnchor)
            : 0;
          const anchor = (anchorIndex >= 0) ? anchorIndex : 0;
          const start = Math.min(anchor, index);
          const end = Math.max(anchor, index);
          for (let i = start; i <= end; i++) {
            if (displaySnapshots[i]) this._selectedIds.add(displaySnapshots[i].name);
          }
        }

        this._updateSelectionVisual(container);
        this._updateSelectionBar();
      });

      // Restore / Delete via component events
      card.addEventListener('snapshot-restore', () => {
        this._restoreSnapshot(card.getAttribute('name'));
      });

      card.addEventListener('snapshot-delete', () => {
        this._deleteSnapshot(card.getAttribute('name'));
      });
    });

    // Ctrl+A and Escape
    this.setAttribute('tabindex', '0');
    this.style.outline = 'none';
    const keydownHandler = (event) => {
      if ((event.ctrlKey || event.metaKey) && event.key === 'a') {
        event.preventDefault();
        for (const s of displaySnapshots) this._selectedIds.add(s.name);
        if (displaySnapshots.length > 0)
          this._lastSelectedAnchor = displaySnapshots[displaySnapshots.length - 1].name;
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
    container.querySelectorAll('aeor-snapshot-card').forEach((card) => {
      if (this._selectedIds.has(card.getAttribute('name')))
        card.setAttribute('selected', '');
      else
        card.removeAttribute('selected');
    });
  }

  _updateSelectionBar() {
    const bar = this.querySelector('#snap-selection-bar');
    if (!bar) return;

    if (this._selectedIds.size > 0) {
      bar.innerHTML = `
        <span class="sel-count">${this._selectedIds.size} selected</span>
        <aeor-confirm-button id="delete-selected-btn" class="confirm-button-danger" label="Delete ${this._selectedIds.size} Snapshot${this._selectedIds.size > 1 ? 's' : ''}" confirmed-text="Deleted!" duration="1000"></aeor-confirm-button>
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

  async _restoreSnapshot(name) {
    try {
      const response = await window.api('/versions/restore', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name }),
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

  async _deleteSnapshot(name) {
    try {
      const response = await window.api(`/versions/snapshots/${encodeURIComponent(name)}`, {
        method: 'DELETE',
      });
      if (!response.ok) {
        const err = await response.json().catch(() => ({ error: 'Delete failed' }));
        throw new Error(err.error || `HTTP ${response.status}`);
      }
      this._snapshots = this._snapshots.filter((s) => s.name !== name);
      this._selectedIds.delete(name);
      this.renderContent();
    } catch (error) {
      if (window.aeorToast) window.aeorToast('Delete failed: ' + error.message, 'error');
    }
  }

  async _deleteSelected() {
    const names = [...this._selectedIds];
    for (const name of names) {
      try {
        await window.api(`/versions/snapshots/${encodeURIComponent(name)}`, { method: 'DELETE' });
        this._snapshots = this._snapshots.filter((s) => s.name !== name);
        this._selectedIds.delete(name);
      } catch (error) {
        if (window.aeorToast) window.aeorToast('Delete failed: ' + error.message, 'error');
      }
    }
    this._lastSelectedAnchor = null;
    this.renderContent();
  }
}

customElements.define('aeor-snapshots', AeorSnapshots);
