'use strict';

import { AeorAdminPage } from '/shared/components/aeor-admin-page.js';
import { elements } from '/aeor/elements.js';
import '/shared/components/aeor-snapshot-card.js';

const { label, input, button } = elements;
const aeorSnapshotCard = elements['aeor-snapshot-card'];
const aeorConfirmButton = elements['aeor-confirm-button'];

const ROOT_USER_ID = '00000000-0000-0000-0000-000000000000';

/** Build a DocumentFragment from a list of ElementDefinition nodes. */
function fragment(doc, ...defs) {
  const frag = doc.createDocumentFragment();
  for (const d of defs) {
    if (d == null) continue;
    frag.appendChild(d.build(doc));
  }
  return frag;
}

function isRoot() {
  return window.AUTH
    && window.AUTH.currentUserId
    && window.AUTH.currentUserId() === ROOT_USER_ID;
}

class AeorSnapshotsPage extends AeorAdminPage {

  constructor() {
    super();
    this._newestName = null;
    this._eventSource = null;
  }

  // ── Subclass contract ───────────────────────────────────────────────

  get title() { return 'Snapshots'; }
  get showCreateButton() { return false; }

  connectedCallback() {
    super.connectedCallback();
    this._connectSSE();
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    if (this._eventSource) {
      this._eventSource.close();
      this._eventSource = null;
    }
  }

  _connectSSE() {
    if (!window.AUTH || !window.AUTH.token) return;
    const url = '/system/events?events=versions_created,versions_deleted,versions_restored'
      + '&token=' + encodeURIComponent(window.AUTH.token);
    try {
      this._eventSource = new EventSource(url);
      const refresh = () => { this._loadItems().catch(() => {}); };
      this._eventSource.addEventListener('versions_created',  refresh);
      this._eventSource.addEventListener('versions_deleted',  refresh);
      this._eventSource.addEventListener('versions_restored', refresh);
      this._eventSource.onerror = () => {
        // Drop the stream on error so the browser will retry only via reload.
        // _loadItems still works manually; we don't want a flapping SSE to
        // hammer the server.
        if (this._eventSource) {
          this._eventSource.close();
          this._eventSource = null;
        }
      };
    } catch (_) {
      // EventSource unsupported — page falls back to manual reload as before
    }
  }

  async fetchItems() {
    const response = await window.api('/versions/snapshots');
    if (!response.ok) throw new Error(`Failed to fetch snapshots (${response.status})`);
    const data = await response.json();

    // Sort newest first, then deduplicate by root hash (id) — keep newest per hash
    const sorted = (data.items || []).sort((a, b) => b.created_at - a.created_at);
    const seen = new Set();
    const deduped = sorted.filter((s) => {
      if (seen.has(s.id)) return false;
      seen.add(s.id);
      return true;
    });

    this._newestName = deduped.length > 0 ? deduped[0].name : null;
    return deduped;
  }

  getItemId(item) {
    return item.name;
  }

  matchesSearch(item, query) {
    return (item.name || '').toLowerCase().includes(query)
      || (item.id || '').toLowerCase().includes(query);
  }

  shouldShowEditButton(items) {
    // Renaming a snapshot is admin-only — non-root cannot edit.
    return isRoot() && items.length === 1;
  }

  // ── Card rendering ──────────────────────────────────────────────────

  renderCard(item) {
    const isCurrent = item.name === this._newestName;
    const age = item.created_at ? this._timeAgo(item.created_at) : '';
    const created = item.created_at ? 'Created ' + new Date(item.created_at).toLocaleString() : '';

    let card = aeorSnapshotCard
      .name(item.name || '')
      .snapshotId(item.id || '')
      .date(created)
      .truncateId(true);

    if (isCurrent) {
      card = card.current(true);
    } else {
      card = card.badge(age);
    }

    return fragment(document, card());
  }

  updateCardSelection(cardEl, isSelected) {
    const snapCard = cardEl.querySelector('aeor-snapshot-card');
    if (snapCard) {
      if (isSelected) snapCard.setAttribute('selected', '');
      else snapCard.removeAttribute('selected');
    }
    super.updateCardSelection(cardEl, isSelected);
  }

  // ── Action bar ──────────────────────────────────────────────────────

  getActionButtons(selectedItems) {
    // Only root can restore/delete snapshots — these are db-wide operations.
    if (!isRoot()) return fragment(document);

    if (selectedItems.length === 1) {
      return fragment(document,
        button.class('secondary small admin-restore-btn')('Restore'),
        aeorConfirmButton
          .class('admin-delete-btn confirm-button-danger')
          .label('Delete')
          .confirmedText('Deleted!')
          .duration('1000')(),
      );
    }

    return fragment(document,
      aeorConfirmButton
        .class('admin-delete-btn confirm-button-danger')
        .label(`Delete ${selectedItems.length} Snapshots`)
        .confirmedText('Deleted!')
        .duration('1000')(),
    );
  }

  _bindActionBarEvents(bar, selectedItems) {
    // Restore button (single selection only)
    const restoreBtn = bar.querySelector('.admin-restore-btn');
    if (restoreBtn && selectedItems.length === 1) {
      restoreBtn.addEventListener('click', async () => {
        try {
          const response = await window.api('/versions/restore', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ name: selectedItems[0].name }),
          });
          if (!response.ok) {
            const err = await response.json().catch(() => ({ error: 'Restore failed' }));
            throw new Error(err.error || `HTTP ${response.status}`);
          }
          if (window.aeorToast) window.aeorToast('HEAD restored to snapshot', 'success');
          this._clearSelection();
          await this._loadItems();
        } catch (error) {
          if (window.aeorToast) window.aeorToast('Restore failed: ' + error.message, 'error');
        }
      });
    }

    // Delete confirm button
    const deleteBtn = bar.querySelector('.admin-delete-btn');
    if (deleteBtn) {
      deleteBtn.addEventListener('confirm', async () => {
        for (const item of selectedItems) {
          try {
            const response = await window.api(`/versions/snapshots/${encodeURIComponent(item.name)}`, {
              method: 'DELETE',
            });
            if (!response.ok) {
              const err = await response.json().catch(() => ({ error: 'Delete failed' }));
              throw new Error(err.error || `HTTP ${response.status}`);
            }
          } catch (error) {
            if (window.aeorToast) window.aeorToast('Delete failed: ' + error.message, 'error');
          }
        }
        this._clearSelection();
        await this._loadItems();
      });
    }
  }

  // ── Edit modal ──────────────────────────────────────────────────────

  renderEditForm(items) {
    return fragment(document,
      label.class('form-label')('Name'),
      input.class('form-input').type('text').name('name').value(items[0].name || '')(),
    );
  }

  async submitEdit(items, modal) {
    const nameInput = modal.querySelector('input[name="name"]');
    const newName = nameInput ? nameInput.value.trim() : '';
    if (!newName) throw new Error('Name is required');

    const response = await window.api(`/versions/snapshots/${encodeURIComponent(items[0].name)}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name: newName }),
    });
    if (!response.ok) {
      const err = await response.json().catch(() => ({ error: 'Rename failed' }));
      throw new Error(err.error || `HTTP ${response.status}`);
    }
  }

  // ── Helpers ─────────────────────────────────────────────────────────

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
}

customElements.define('aeor-snapshots', AeorSnapshotsPage);
