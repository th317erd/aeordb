'use strict';

import { AeorAdminPage } from '/shared/components/aeor-admin-page.js';
import { escapeHtml } from '/shared/utils.js';

class AeorUsersPage extends AeorAdminPage {

  // ── Subclass contract ───────────────────────────────────────────────

  get title() { return 'Users'; }
  get showCreateButton() { return true; }

  async fetchItems() {
    const response = await window.api('/system/users');
    if (!response.ok)
      throw new Error(`Failed to fetch users (${response.status})`);
    const data = await response.json();
    return data.items || [];
  }

  getItemId(item) {
    return item.user_id || item.id;
  }

  renderCard(item) {
    return `
      <div class="admin-card-header">
        <div class="admin-card-title">
          ${escapeHtml(item.username)}
          <span class="badge ${item.is_active ? 'badge-active' : 'badge-inactive'}">
            ${item.is_active ? 'Active' : 'Inactive'}
          </span>
        </div>
      </div>
      <div class="admin-card-meta">${escapeHtml(item.email || '')}</div>
      <div class="admin-card-meta">Created ${item.created_at ? new Date(item.created_at).toLocaleDateString() : '\u2014'}</div>
    `;
  }

  matchesSearch(item, query) {
    const username = (item.username || '').toLowerCase();
    const email = (item.email || '').toLowerCase();
    return username.includes(query) || email.includes(query);
  }

  shouldShowEditButton(selectedItems) {
    return selectedItems.length === 1;
  }

  getActionButtons(selectedItems) {
    const count = selectedItems.length;
    const label = count === 1 ? 'Deactivate' : `Deactivate ${count}`;
    return `<aeor-confirm-button class="confirm-button-danger small admin-deactivate-btn">${label}</aeor-confirm-button>`;
  }

  _bindActionBarEvents(bar, selectedItems) {
    const deactivateBtn = bar.querySelector('.admin-deactivate-btn');
    if (!deactivateBtn) return;

    deactivateBtn.addEventListener('confirm', async () => {
      for (const item of selectedItems) {
        const userId = this.getItemId(item);
        try {
          const response = await window.api(`/system/users/${userId}`, { method: 'DELETE' });
          if (!response.ok) {
            const text = await response.text();
            throw new Error(text || `Deactivate failed (${response.status})`);
          }
        } catch (error) {
          if (window.aeorToast)
            window.aeorToast(`Deactivate failed: ${error.message}`, 'error');
        }
      }

      this._clearSelection();
      await this._loadItems();
      if (window.aeorToast)
        window.aeorToast('User(s) deactivated', 'success');
    });
  }

  // ── Create modal ────────────────────────────────────────────────────

  renderCreateForm() {
    return `
      <div class="modal-field-group">
        <label class="modal-field-label" for="create-username">Username</label>
        <input class="form-input" id="create-username" type="text" required>
      </div>
      <div class="modal-field-group">
        <label class="modal-field-label" for="create-email">Email</label>
        <input class="form-input" id="create-email" type="text" required>
      </div>
    `;
  }

  async submitCreate(modal) {
    const username = modal.querySelector('#create-username').value.trim();
    const email = modal.querySelector('#create-email').value.trim();

    if (!username || !email)
      throw new Error('Username and email are required');

    const response = await window.api('/system/users', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ username, email }),
    });

    if (!response.ok) {
      const text = await response.text();
      throw new Error(text || `Create failed (${response.status})`);
    }

    return response.json();
  }

  // ── Edit modal ──────────────────────────────────────────────────────

  renderEditForm(items) {
    const user = items[0];
    const isActive = user.is_active ? 'checked' : '';

    return `
      <div class="modal-field-group">
        <label class="modal-field-label" for="edit-username">Username</label>
        <input class="form-input" id="edit-username" type="text" value="${escapeHtml(user.username || '')}" required>
      </div>
      <div class="modal-field-group">
        <label class="modal-field-label" for="edit-email">Email</label>
        <input class="form-input" id="edit-email" type="text" value="${escapeHtml(user.email || '')}" required>
      </div>
      <div class="modal-field-group">
        <div class="toggle-wrap">
          <label class="toggle">
            <input type="checkbox" id="edit-active" ${isActive}>
            <span class="toggle-track"></span>
          </label>
          <span class="modal-field-label toggle-label">Active</span>
        </div>
      </div>
    `;
  }

  async submitEdit(items, modal) {
    const user = items[0];
    const userId = this.getItemId(user);
    const username = modal.querySelector('#edit-username').value.trim();
    const email = modal.querySelector('#edit-email').value.trim();
    const is_active = modal.querySelector('#edit-active').checked;

    if (!username || !email)
      throw new Error('Username and email are required');

    const response = await window.api(`/system/users/${userId}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ username, email, is_active }),
    });

    if (!response.ok) {
      const text = await response.text();
      throw new Error(text || `Update failed (${response.status})`);
    }

    return response.json();
  }
}

customElements.define('aeor-users', AeorUsersPage);
