'use strict';

import { escapeHtml } from '/system/portal/shared/utils.js';

class AeorKeys extends HTMLElement {
  constructor() {
    super();
    this._keys = [];
    this._allKeys = null;
    this._error = null;
    this._searchQuery = '';
    this._currentKeyId = null;
    this._isRoot = false;
    this._selectedKeyIds = new Set();
    this._lastSelectedAnchor = null;
  }

  connectedCallback() {
    this._currentKeyId = this._getCurrentKeyId();
    this._isRoot = this._checkIsRoot();
    this.render();
    this._fetchOwnKeys();
  }

  _getCurrentKeyId() {
    try {
      const token = window.AUTH.token;
      if (!token) return null;
      const payload = JSON.parse(atob(token.split('.')[1]));
      return payload.key_id || null;
    } catch { return null; }
  }

  _checkIsRoot() {
    try {
      const token = window.AUTH.token;
      if (!token) return false;
      const payload = JSON.parse(atob(token.split('.')[1]));
      return payload.sub === '00000000-0000-0000-0000-000000000000';
    } catch { return false; }
  }

  render() {
    this.innerHTML = `
      <div class="page-header">
        <h1 class="page-title">Keys</h1>
        <button class="button button-primary" id="create-key-button">Create Key</button>
      </div>
      <div style="margin-bottom:16px;">
        <input class="form-input" id="keys-search" type="text"
          placeholder="Showing your keys. Search here to show others.">
      </div>
      <div id="keys-error"></div>
      <div id="keys-content"></div>
      <div id="keys-modal"></div>
    `;

    this.querySelector('#create-key-button').addEventListener('click', () => this.showCreateModal());
    this.querySelector('#keys-search').addEventListener('input', (event) => this._onSearch(event.target.value));
  }

  async _fetchOwnKeys() {
    try {
      const response = await window.api('/auth/keys');

      if (!response.ok)
        throw new Error(`Failed to fetch keys (${response.status})`);

      const data = await response.json();
      this._keys = data.items || data;
      this._error = null;
      this.renderContent();
    } catch (error) {
      this._error = error.message;
      this.renderContent();
    }
  }

  async _fetchAllKeys() {
    try {
      const endpoint = this._isRoot ? '/auth/keys/admin' : '/auth/keys';
      const response = await window.api(endpoint);

      if (!response.ok)
        throw new Error(`Failed to fetch keys (${response.status})`);

      const data = await response.json();
      this._allKeys = data.items || data;
      this._error = null;
    } catch (error) {
      this._error = error.message;
      this._allKeys = [];
    }
  }

  async _onSearch(query) {
    this._searchQuery = query;

    if (query.length === 0) {
      // Back to own keys view
      this._keys = [];
      await this._fetchOwnKeys();
      return;
    }

    // Lazy-load all keys on first search
    if (this._allKeys === null) {
      await this._fetchAllKeys();
    }

    this.renderContent();
  }

  _getDisplayKeys() {
    if (this._searchQuery.length === 0) {
      return this._keys;
    }

    const source = this._allKeys || [];
    const query = this._searchQuery.trim().toLowerCase();

    // Empty after trim (e.g. just a space) = show all, no filtering
    if (query.length === 0) return source;

    return source.filter((key) => {
      const label = (key.label || '').toLowerCase();
      const keyId = (key.key_id || '').toLowerCase();
      const userId = (key.user_id || '').toLowerCase();
      const rulesStr = JSON.stringify(key.rules || []).toLowerCase();
      return label.includes(query) || keyId.includes(query) || userId.includes(query) || rulesStr.includes(query);
    });
  }

  _getStatus(key) {
    if (key.key_id === this._currentKeyId) {
      return { text: 'Current Session', cssClass: 'badge-session' };
    }
    if (key.is_revoked) {
      return { text: 'Revoked', cssClass: 'badge-inactive' };
    }
    if (key.expires_at && key.expires_at < Date.now()) {
      return { text: 'Expired', cssClass: 'badge-expired' };
    }
    return { text: 'Active', cssClass: 'badge-active' };
  }

  _truncateId(id) {
    if (!id) return '\u2014';
    const str = String(id);
    if (str.length <= 12) return str;
    return str.substring(0, 8) + '\u2026';
  }

  renderContent() {
    const contentContainer = this.querySelector('#keys-content');
    const errorContainer = this.querySelector('#keys-error');

    if (!contentContainer || !errorContainer)
      return;

    if (this._error) {
      errorContainer.innerHTML = `<div class="alert alert-error">${escapeHtml(this._error)}</div>`;
    } else {
      errorContainer.innerHTML = '';
    }

    const displayKeys = this._getDisplayKeys();

    if (displayKeys.length === 0 && !this._error) {
      contentContainer.innerHTML = `
        <div class="card" style="text-align:center;padding:40px;">
          <div style="color:#8b949e;">No keys found.</div>
        </div>
      `;
      return;
    }

    contentContainer.innerHTML = `
      <style>
        .badge-expired { background: rgba(210, 153, 34, 0.15); color: var(--warning); }
        .badge-session { background: rgba(249, 115, 22, 0.15); color: var(--accent); }

        .key-list { display: flex; flex-direction: column; gap: 1px; }

        .key-row {
          display: grid;
          grid-template-columns: 1fr auto auto;
          align-items: center;
          gap: 12px;
          padding: 12px 16px;
          background: var(--card);
          border: 1px solid var(--border);
          border-radius: 6px;
          margin-bottom: 4px;
          cursor: default;
        }

        .key-row { cursor: pointer; outline: none; user-select: none; }
        .key-row:hover { border-color: var(--accent); }
        .key-row.selected { background: rgba(249, 115, 22, 0.15); border-color: var(--accent); }

        .keys-selection-bar {
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

        .keys-selection-bar .sel-count { font-weight: 600; color: var(--text); }

        .key-info { min-width: 0; }

        .key-label {
          font-weight: 600;
          color: var(--text);
          margin-bottom: 2px;
          display: flex;
          align-items: center;
          gap: 8px;
          flex-wrap: wrap;
        }

        .key-id {
          font-family: var(--font-mono);
          font-size: 0.8rem;
          color: var(--text-muted);
        }

        .key-meta {
          font-size: 0.78rem;
          color: var(--text-muted);
          font-family: var(--font-mono);
          margin-top: 4px;
        }

        .key-user {
          font-family: var(--font-mono);
          font-size: 0.8rem;
          color: var(--text-muted);
          text-align: right;
        }

        @media (max-width: 768px) {
          .key-row {
            grid-template-columns: 1fr auto;
          }
          .key-user { display: none; }
        }
      </style>

      <div class="keys-selection-bar" id="keys-selection-bar">&nbsp;</div>
      <div class="key-list">
        ${displayKeys.map((key) => {
          const status = this._getStatus(key);
          const isSelected = this._selectedKeyIds.has(key.key_id);
          const created = (key.created_at) ? new Date(key.created_at).toLocaleDateString() : '\u2014';
          const expires = (key.expires_at) ? new Date(key.expires_at).toLocaleDateString() : '\u2014';

          return `
            <div class="key-row ${isSelected ? 'selected' : ''}" data-key-id="${escapeHtml(String(key.key_id || ''))}">
              <div class="key-info">
                <div class="key-label">
                  ${escapeHtml(key.label || 'Unnamed Key')}
                  <span class="badge ${status.cssClass}">${escapeHtml(status.text)}</span>
                </div>
                <div class="key-id" title="${escapeHtml(String(key.key_id || ''))}">${escapeHtml(this._truncateId(key.key_id))}</div>
                <div class="key-meta">Created ${created} \u00B7 Expires ${expires}</div>
              </div>
              <div class="key-user" title="${escapeHtml(String(key.user_id || ''))}">${escapeHtml(this._truncateId(key.user_id))}</div>
              <div></div>
            </div>
          `;
        }).join('')}
      </div>
    `;

    this._bindKeyRowEvents(contentContainer, displayKeys);
    this._updateSelectionBar();
  }

  _bindKeyRowEvents(container, displayKeys) {
    container.querySelectorAll('.key-row').forEach((row) => {
      row.addEventListener('click', (event) => {
        // Don't select when clicking buttons inside the row
        if (event.target.closest('button')) return;

        const keyId = row.dataset.keyId;
        const index = displayKeys.findIndex((k) => k.key_id === keyId);
        const isMobile = window.innerWidth <= 768;
        const isCtrl = isMobile || event.ctrlKey || event.metaKey;
        const isShift = !isMobile && event.shiftKey;

        if (!isCtrl && !isShift) {
          // Plain click — single select
          this._selectedKeyIds.clear();
          this._selectedKeyIds.add(keyId);
          this._lastSelectedAnchor = keyId;
        } else if (isCtrl) {
          // Ctrl+click — toggle
          if (this._selectedKeyIds.has(keyId))
            this._selectedKeyIds.delete(keyId);
          else
            this._selectedKeyIds.add(keyId);
          this._lastSelectedAnchor = keyId;
        } else if (isShift) {
          // Shift+click — range select
          const anchorIndex = this._lastSelectedAnchor
            ? displayKeys.findIndex((k) => k.key_id === this._lastSelectedAnchor)
            : 0;
          const anchor = (anchorIndex >= 0) ? anchorIndex : 0;
          const start = Math.min(anchor, index);
          const end = Math.max(anchor, index);
          for (let i = start; i <= end; i++) {
            if (displayKeys[i])
              this._selectedKeyIds.add(displayKeys[i].key_id);
          }
        }

        this._updateSelectionVisual(container);
        this._updateSelectionBar();
      });
    });

    // Ctrl+A and Escape
    this.setAttribute('tabindex', '0');
    this.style.outline = 'none';
    const keydownHandler = (event) => {
      if ((event.ctrlKey || event.metaKey) && event.key === 'a') {
        event.preventDefault();
        for (const key of displayKeys)
          this._selectedKeyIds.add(key.key_id);
        if (displayKeys.length > 0)
          this._lastSelectedAnchor = displayKeys[displayKeys.length - 1].key_id;
        this._updateSelectionVisual(container);
        this._updateSelectionBar();
      } else if (event.key === 'Escape') {
        this._selectedKeyIds.clear();
        this._lastSelectedAnchor = null;
        this._updateSelectionVisual(container);
        this._updateSelectionBar();
      }
    };

    if (this._keydownHandler)
      this.removeEventListener('keydown', this._keydownHandler);
    this._keydownHandler = keydownHandler;
    this.addEventListener('keydown', keydownHandler);
  }

  _updateSelectionVisual(container) {
    container.querySelectorAll('.key-row').forEach((row) => {
      if (this._selectedKeyIds.has(row.dataset.keyId))
        row.classList.add('selected');
      else
        row.classList.remove('selected');
    });
  }

  _updateSelectionBar() {
    const bar = this.querySelector('#keys-selection-bar');
    if (!bar) return;

    if (this._selectedKeyIds.size > 0) {
      // Count how many selected keys are revocable (not already revoked)
      const displayKeys = this._getDisplayKeys();
      const revocableCount = [...this._selectedKeyIds].filter((id) => {
        const key = displayKeys.find((k) => k.key_id === id);
        return key && !key.is_revoked;
      }).length;

      bar.innerHTML = `
        <span class="sel-count">${this._selectedKeyIds.size} selected</span>
        ${revocableCount > 0 ? `<button class="button button-small button-danger" id="revoke-selected-btn">Revoke ${revocableCount} Key${revocableCount > 1 ? 's' : ''}</button>` : ''}
        <button class="button button-small" id="clear-selection-btn">Clear</button>
      `;
      bar.style.visibility = 'visible';

      const revokeBtn = bar.querySelector('#revoke-selected-btn');
      if (revokeBtn) {
        revokeBtn.addEventListener('click', () => this._revokeSelected());
      }
      bar.querySelector('#clear-selection-btn').addEventListener('click', () => {
        this._selectedKeyIds.clear();
        this._lastSelectedAnchor = null;
        const container = this.querySelector('#keys-content');
        if (container) this._updateSelectionVisual(container);
        this._updateSelectionBar();
      });
    } else {
      bar.innerHTML = '&nbsp;';
      bar.style.visibility = 'hidden';
    }
  }

  async _revokeSelected() {
    const displayKeys = this._getDisplayKeys();
    const toRevoke = [...this._selectedKeyIds].filter((id) => {
      const key = displayKeys.find((k) => k.key_id === id);
      return key && !key.is_revoked;
    });

    if (toRevoke.length === 0) return;

    // Check if the current session key is being revoked
    const revokingCurrentSession = toRevoke.includes(this._currentKeyId);
    const warningMessage = revokingCurrentSession
      ? `Revoke ${toRevoke.length} key${toRevoke.length > 1 ? 's' : ''}?\n\nWARNING: This includes your current session key. You will be logged out immediately.`
      : `Revoke ${toRevoke.length} key${toRevoke.length > 1 ? 's' : ''}? This action cannot be undone.`;

    const confirmed = await this._confirmAction('Revoke Keys', warningMessage);
    if (!confirmed) return;

    for (const keyId of toRevoke) {
      try {
        const endpoint = this._isRoot && this._searchQuery.length > 0
          ? `/auth/keys/admin/${keyId}`
          : `/auth/keys/${keyId}`;
        await window.api(endpoint, { method: 'DELETE' });
      } catch (error) {
        if (window.aeorToast)
          window.aeorToast(`Revoke failed for ${this._truncateId(keyId)}: ${error.message}`, 'error');
      }
    }

    // If we revoked the current session key, log out
    if (revokingCurrentSession) {
      window.AUTH.clear();
      window.location.href = '/system/portal';
      return;
    }

    this._selectedKeyIds.clear();
    this._lastSelectedAnchor = null;
    this._allKeys = null;
    await this._fetchOwnKeys();
    if (this._searchQuery.length > 0) {
      await this._fetchAllKeys();
      this.renderContent();
    }
  }

  _confirmAction(title, message) {
    return new Promise((resolve) => {
      const modalContainer = this.querySelector('#keys-modal');
      if (!modalContainer) { resolve(false); return; }

      modalContainer.innerHTML = `
        <div class="modal-overlay" id="modal-overlay">
          <div class="modal-content">
            <div class="modal-title">${escapeHtml(title)}</div>
            <p style="color:var(--text-muted);margin-bottom:18px;">${escapeHtml(message)}</p>
            <div class="modal-actions">
              <button class="button" type="button" id="confirm-cancel">Cancel</button>
              <button class="button button-danger" type="button" id="confirm-ok">Revoke</button>
            </div>
          </div>
        </div>
      `;

      let resolved = false;
      const done = (result) => {
        if (resolved) return;
        resolved = true;
        modalContainer.innerHTML = '';
        resolve(result);
      };

      modalContainer.querySelector('#confirm-cancel').addEventListener('click', () => done(false));
      modalContainer.querySelector('#confirm-ok').addEventListener('click', () => done(true));
      modalContainer.querySelector('#modal-overlay').addEventListener('click', (event) => {
        if (event.target.id === 'modal-overlay') done(false);
      });
    });
  }

  _confirmRevoke(keyId) {
    const modalContainer = this.querySelector('#keys-modal');
    if (!modalContainer)
      return;

    modalContainer.innerHTML = `
      <div class="modal-overlay" id="modal-overlay">
        <div class="modal-content">
          <div class="modal-title">Revoke Key</div>
          <p style="color:var(--text-muted);margin-bottom:18px;">
            Are you sure you want to revoke key <code style="font-family:var(--font-mono);color:var(--text);">${escapeHtml(this._truncateId(keyId))}</code>?
            This action cannot be undone.
          </p>
          <div class="modal-actions">
            <button class="button" type="button" id="cancel-revoke-button">Cancel</button>
            <button class="button button-danger" type="button" id="confirm-revoke-button">Revoke</button>
          </div>
        </div>
      </div>
    `;

    modalContainer.querySelector('#cancel-revoke-button').addEventListener('click', () => this.closeModal());
    modalContainer.querySelector('#modal-overlay').addEventListener('click', (event) => {
      if (event.target.id === 'modal-overlay')
        this.closeModal();
    });
    modalContainer.querySelector('#confirm-revoke-button').addEventListener('click', () => this._revokeKey(keyId));
  }

  async _revokeKey(keyId) {
    try {
      const endpoint = this._isRoot && this._searchQuery.length > 0
        ? `/auth/keys/admin/${keyId}`
        : `/auth/keys/${keyId}`;

      const response = await window.api(endpoint, { method: 'DELETE' });

      if (!response.ok) {
        const text = await response.text();
        throw new Error(text || `Revoke failed (${response.status})`);
      }

      this.closeModal();
      // Invalidate cached all-keys so next search re-fetches
      this._allKeys = null;
      await this._fetchOwnKeys();
      // Re-apply search if active
      if (this._searchQuery.length > 0) {
        await this._fetchAllKeys();
        this.renderContent();
      }
    } catch (error) {
      const errorContainer = this.querySelector('#keys-error');
      if (errorContainer)
        errorContainer.innerHTML = `<div class="alert alert-error">${escapeHtml(error.message)}</div>`;
      this.closeModal();
    }
  }

  showCreateModal() {
    const modalContainer = this.querySelector('#keys-modal');
    if (!modalContainer)
      return;

    modalContainer.innerHTML = `
      <div class="modal-overlay" id="modal-overlay">
        <div class="modal-content">
          <div class="modal-title">Create Key</div>
          <div id="modal-error"></div>
          <form id="create-key-form">
            <div class="form-group">
              <label class="form-label" for="create-label">Label (optional)</label>
              <input class="form-input" id="create-label" type="text" placeholder="e.g. CI pipeline key">
            </div>
            <div class="form-group">
              <label class="form-label" for="create-expires">Expires in (days)</label>
              <input class="form-input" id="create-expires" type="number" value="365" min="1" max="3650">
            </div>
            <div class="modal-actions">
              <button class="button" type="button" id="cancel-create-button">Cancel</button>
              <button class="button button-primary" type="submit">Create</button>
            </div>
          </form>
        </div>
      </div>
    `;

    modalContainer.querySelector('#cancel-create-button').addEventListener('click', () => this.closeModal());
    modalContainer.querySelector('#modal-overlay').addEventListener('click', (event) => {
      if (event.target.id === 'modal-overlay')
        this.closeModal();
    });
    modalContainer.querySelector('#create-key-form').addEventListener('submit', (event) => this._handleCreate(event));
  }

  async _handleCreate(event) {
    event.preventDefault();
    const modalError = this.querySelector('#modal-error');
    const labelInput = this.querySelector('#create-label');
    const expiresInput = this.querySelector('#create-expires');

    const body = {
      expires_in_days: parseInt(expiresInput.value, 10) || 365,
    };

    if (labelInput.value.trim()) {
      body.label = labelInput.value.trim();
    }

    try {
      const response = await window.api('/auth/keys', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      });

      if (!response.ok) {
        const text = await response.text();
        throw new Error(text || `Create failed (${response.status})`);
      }

      const data = await response.json();
      this.closeModal();
      this._showKeyResult(data);
      // Invalidate cached all-keys
      this._allKeys = null;
      this._fetchOwnKeys();
    } catch (error) {
      if (modalError)
        modalError.innerHTML = `<div class="alert alert-error">${escapeHtml(error.message)}</div>`;
    }
  }

  _showKeyResult(data) {
    const modalContainer = this.querySelector('#keys-modal');
    if (!modalContainer)
      return;

    modalContainer.innerHTML = `
      <div class="modal-overlay" id="modal-overlay">
        <div class="modal-content">
          <div class="modal-title">Key Created</div>
          <div class="alert" style="background:rgba(210,153,34,0.12);border:1px solid rgba(210,153,34,0.3);color:var(--warning);margin-bottom:16px;">
            This key will not be shown again. Copy it now and store it securely.
          </div>
          <div class="form-group">
            <label class="form-label">API Key</label>
            <div style="display:flex;gap:8px;">
              <input class="form-input" id="created-key-value" type="text" readonly
                value="${escapeHtml(data.key || '')}"
                style="font-family:var(--font-mono);font-size:0.85rem;">
              <button class="button button-primary" type="button" id="copy-key-button">Copy</button>
            </div>
          </div>
          ${data.label ? `<div style="color:var(--text-muted);font-size:0.85rem;margin-bottom:8px;">Label: ${escapeHtml(data.label)}</div>` : ''}
          <div style="color:var(--text-muted);font-size:0.85rem;margin-bottom:8px;">Key ID: <code style="font-family:var(--font-mono);">${escapeHtml(String(data.key_id || ''))}</code></div>
          <div style="color:var(--text-muted);font-size:0.85rem;margin-bottom:16px;">Expires: ${data.expires_at ? new Date(data.expires_at).toLocaleDateString() : '\u2014'}</div>
          <div class="modal-actions">
            <button class="button button-primary" type="button" id="close-result-button">Done</button>
          </div>
        </div>
      </div>
    `;

    modalContainer.querySelector('#copy-key-button').addEventListener('click', () => {
      const input = modalContainer.querySelector('#created-key-value');
      if (input) {
        navigator.clipboard.writeText(input.value).then(() => {
          const btn = modalContainer.querySelector('#copy-key-button');
          if (btn) {
            btn.textContent = 'Copied!';
            setTimeout(() => { btn.textContent = 'Copy'; }, 2000);
          }
        }).catch(() => {
          input.select();
        });
      }
    });

    modalContainer.querySelector('#close-result-button').addEventListener('click', () => this.closeModal());
    modalContainer.querySelector('#modal-overlay').addEventListener('click', (event) => {
      if (event.target.id === 'modal-overlay')
        this.closeModal();
    });
  }

  closeModal() {
    const modalContainer = this.querySelector('#keys-modal');
    if (modalContainer)
      modalContainer.innerHTML = '';
  }
}

customElements.define('aeor-keys', AeorKeys);
